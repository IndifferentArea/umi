use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    fmt,
    mem::{self, MaybeUninit},
    ops::{Bound, Range, RangeBounds},
};

use futures_util::{future::try_join_all, stream, FutureExt, Stream, StreamExt, TryStreamExt};
use ksc_core::Error::{self, EINVAL, ENOSPC};
use umifs::traits::{Io, IoExt};

use crate::raw::BiosParameterBlock;

pub const RESERVED_FAT_ENTRIES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FatEntry {
    Free,
    Next(u32),
    Bad,
    End,
}

impl FatEntry {
    pub fn from_raw(raw: u32, cluster: u32) -> Self {
        match raw & 0x0fff_ffff {
            0 if (0x0FFF_FFF7..=0x0FFF_FFFF).contains(&cluster) => {
                let tmp = if cluster == 0x0FFF_FFF7 {
                    "BAD_CLUSTER"
                } else {
                    "end-of-chain"
                };
                log::warn!(
                    "cluster number {} is a special value in FAT to indicate {}; it should never be seen as free",
                    cluster, tmp
                );
                FatEntry::Bad // avoid accidental use or allocation into a FAT
                              // chain
            }
            0 => FatEntry::Free,
            0x0FFF_FFF7 => FatEntry::Bad,
            0x0FFF_FFF8..=0x0FFF_FFFF => FatEntry::End,
            n if (0x0FFF_FFF7..=0x0FFF_FFFF).contains(&cluster) => {
                let tmp = if cluster == 0x0FFF_FFF7 {
                    "BAD_CLUSTER"
                } else {
                    "end-of-chain"
                };
                log::warn!("cluster number {} is a special value in FAT to indicate {}; hiding potential FAT chain value {} and instead reporting as a bad sector", cluster, tmp, n);
                FatEntry::Bad // avoid accidental use or allocation into a FAT
                              // chain
            }
            n => FatEntry::Next(n),
        }
    }

    pub fn into_raw(self, cluster: u32, old_raw: u32) -> u32 {
        if self == FatEntry::Free && (0x0FFF_FFF7..=0x0FFF_FFFF).contains(&cluster) {
            // NOTE: it is technically allowed for them to store FAT chain loops,
            //       or even have them all store value '4' as their next cluster.
            //       Some believe only FatEntry::Bad should be allowed for this edge case.
            let tmp = if cluster == 0x0FFF_FFF7 {
                "BAD_CLUSTER"
            } else {
                "end-of-chain"
            };
            panic!(
                "cluster number {} is a special value in FAT to indicate {}; it should never be set as free",
                cluster, tmp
            );
        };
        let raw = match self {
            FatEntry::Free => 0,
            FatEntry::Bad => 0x0FFF_FFF7,
            FatEntry::End => 0x0FFF_FFFF,
            FatEntry::Next(n) => n,
        };
        old_raw | raw
    }
}

pub struct Fat {
    device: Arc<dyn Io>,
    start_offset: usize,
    cluster_count: u32,
    mirrors: u8,
}

impl fmt::Debug for Fat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fat")
            .field("start_offset", &self.start_offset)
            .field("cluster_count", &self.cluster_count)
            .field("mirrors", &self.mirrors)
            .finish()
    }
}

impl Fat {
    const ENTRY_SIZE: usize = mem::size_of::<u32>();

    pub fn new(device: Arc<dyn Io>, bpb: &BiosParameterBlock) -> Self {
        let sectors_per_fat = bpb.sectors_per_fat();
        let mirroring_enabled = bpb.mirroring_enabled();
        let (fat_first_sector, mirrors) = if mirroring_enabled {
            (u32::from(bpb.reserved_sectors), bpb.fats)
        } else {
            let active_fat = u32::from(bpb.active_fat());
            let fat_first_sector = u32::from(bpb.reserved_sectors) + active_fat * sectors_per_fat;
            (fat_first_sector, 1)
        };
        Fat {
            device,
            start_offset: bpb.bytes_from_sectors(fat_first_sector) as usize,
            cluster_count: bpb.total_clusters(),
            mirrors,
        }
    }

    pub fn device(&self) -> &Arc<dyn Io> {
        &self.device
    }

    pub const fn size(&self) -> usize {
        self.cluster_count as usize * Self::ENTRY_SIZE
    }

    pub const fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    pub const fn allocable_range(&self) -> Range<u32> {
        RESERVED_FAT_ENTRIES..(self.cluster_count + RESERVED_FAT_ENTRIES)
    }

    fn offset(&self, mirror: u8, cluster: u32) -> usize {
        self.start_offset + self.size() * mirror as usize + cluster as usize * Self::ENTRY_SIZE
    }

    async fn get_raw(&self, cluster: u32) -> Result<u32, Error> {
        let mut buf = [0; 4];
        if cluster >= self.allocable_range().end {
            return Err(EINVAL);
        }
        self.device
            .read_exact_at(self.offset(0, cluster), &mut buf)
            .await?;

        Ok(u32::from_le_bytes(buf))
    }

    /// # Safety
    ///
    /// The buf must be written zeros.
    async unsafe fn get_range_raw(
        &self,
        start: u32,
        buf: &mut [MaybeUninit<u32>],
    ) -> Result<usize, Error> {
        let end = (start + u32::try_from(buf.len())?).min(self.allocable_range().end);
        if start > end {
            return Err(EINVAL);
        }
        if start == end {
            return Ok(0);
        }
        let read_len = (end - start) as usize;
        let bytes = MaybeUninit::slice_as_bytes_mut(&mut buf[0..read_len]);

        self.device
            .read_exact_at(self.offset(0, start), unsafe {
                MaybeUninit::slice_assume_init_mut(bytes)
            })
            .await?;

        Ok(read_len)
    }

    pub async fn get_range<'a>(
        &self,
        start: u32,
        buf: &'a mut [u32],
    ) -> Result<impl Iterator<Item = (u32, FatEntry)> + Send + Clone + 'a, Error> {
        buf.fill(0);
        // SAFETY: init to uninit is safe.
        let len = unsafe { self.get_range_raw(start, mem::transmute(&mut *buf)) }.await?;

        let zip = buf[..len].iter().zip(start..);
        Ok(zip.map(|(&raw, cluster)| (cluster, FatEntry::from_raw(raw, cluster))))
    }

    #[allow(dead_code)]
    pub async fn set_range(
        &self,
        start: u32,
        buf: &mut [u32],
        entry: FatEntry,
    ) -> Result<(), Error> {
        buf.fill(0);
        let len = unsafe { self.get_range_raw(start, mem::transmute(&mut *buf)) }.await?;

        for (raw, cluster) in buf[..len].iter_mut().zip(start..) {
            let old = *raw & 0xf000_0000;
            *raw = entry.into_raw(cluster, old)
        }

        // SAFETY: init to uninit is safe.
        let buf: &[MaybeUninit<u32>] = unsafe { mem::transmute(&buf[..len]) };
        // SAFETY: All bytes are valid.
        let bytes: &[u8] =
            unsafe { MaybeUninit::slice_assume_init_ref(MaybeUninit::slice_as_bytes(buf)) };

        try_join_all((0..self.mirrors).map(|mirror| async move {
            let offset = self.offset(mirror, start);
            self.device.write_all_at(offset, bytes).await
        }))
        .await?;

        Ok(())
    }

    pub async fn get(&self, cluster: u32) -> Result<FatEntry, Error> {
        self.get_raw(cluster)
            .await
            .map(|raw| FatEntry::from_raw(raw, cluster))
    }

    pub async fn set(&self, cluster: u32, entry: FatEntry) -> Result<(), Error> {
        let old = self.get_raw(cluster).await? & 0xf000_0000;
        let raw = entry.into_raw(cluster, old);

        let buffer = &raw.to_le_bytes();
        try_join_all((0..self.mirrors).map(|mirror| async move {
            let offset = self.offset(mirror, cluster);
            self.device.write_all_at(offset, buffer).await
        }))
        .await?;

        Ok(())
    }

    async fn find_free<R>(&self, cluster_range: R) -> Result<u32, Error>
    where
        R: RangeBounds<u32>,
    {
        let allocable_range = self.allocable_range();

        let start = match cluster_range.start_bound() {
            Bound::Included(&bound) => bound.max(allocable_range.start),
            Bound::Excluded(&bound) => bound.wrapping_add(1).max(allocable_range.start),
            Bound::Unbounded => allocable_range.end,
        };
        let end = match cluster_range.end_bound() {
            Bound::Included(&bound) => bound.wrapping_add(1).min(allocable_range.end),
            Bound::Excluded(&bound) => bound.min(allocable_range.end),
            Bound::Unbounded => allocable_range.end,
        };

        let mut buf = [0; BATCH_LEN];

        // The range may be massive so that `try_join_all` will allocate huge amount of
        // memory, reaulting in a potential memory exhaustion.
        for start in (start..end).step_by(BATCH_LEN) {
            let len = BATCH_LEN.min((end - start) as usize);
            for (cluster, entry) in self.get_range(start, &mut buf[..len]).await? {
                if entry == FatEntry::Free {
                    return Ok(cluster);
                }
            }
        }
        Err(ENOSPC)
    }

    pub async fn count_free(&self) -> usize {
        let stream = stream::iter(self.allocable_range())
            .filter(|&cluster| self.get(cluster).map(|res| res.unwrap() == FatEntry::Free));
        stream.count().await
    }

    pub async fn allocate(&self, prev: Option<u32>, hint: Option<u32>) -> Result<u32, Error> {
        let hint = hint.unwrap_or(self.allocable_range().start);

        let ret = match self.find_free(hint..).await {
            Ok(cluster) => cluster,
            Err(ENOSPC) => self.find_free(..hint).await?,
            Err(err) => return Err(err),
        };

        self.set(ret, FatEntry::End).await?;
        if let Some(prev) = prev {
            self.set(prev, FatEntry::Next(ret)).await?;
        }
        Ok(ret)
    }

    async fn iter_next(&self, cluster: u32) -> Result<Option<u32>, Error> {
        Ok(match self.get(cluster).await? {
            FatEntry::Next(next) => Some(next),
            _ => None,
        })
    }

    async fn iter_ranged_next<'a>(
        &self,
        start: u32,
        buf: &'a mut [u32],
    ) -> Result<impl Iterator<Item = u32> + Send + 'a, Error> {
        let iter = self.get_range(start, buf).await?;
        let last = [(u32::MAX, FatEntry::Next(start))]
            .into_iter()
            .chain(iter.clone());
        let zip = last.zip(iter);
        Ok(
            zip.map_while(|((_last_cluster, last_entry), (cluster, entry))| {
                if let FatEntry::Next(last_next) = last_entry {
                    if last_next != cluster {
                        return None;
                    }
                }
                match entry {
                    FatEntry::Next(next) => Some(next),
                    _ => None,
                }
            }),
        )
    }

    pub fn cluster_chain(&self, start: u32) -> impl Stream<Item = Result<u32, Error>> + Send + '_ {
        stream::unfold((self, Some(Ok(start))), |(this, cluster)| async move {
            Some(match cluster? {
                Ok(cluster) => {
                    let next = this.iter_next(cluster).await;
                    (Ok(cluster), (this, next.transpose()))
                }
                Err(err) => (Err(err), (this, None)),
            })
        })
    }

    pub async fn all_clusters(&self, start: u32) -> Result<Vec<(u32, u32)>, Error> {
        let mut buf = [0; BATCH_LEN];
        let mut ret = vec![(start, 0)];
        loop {
            let last_len = ret.len();
            let iter = self
                .iter_ranged_next(ret.last().unwrap().0, &mut buf)
                .await?;
            ret.extend(iter.map(|c| (c, 0)));

            let mut last = None;
            let mut end = None;
            for &mut (c, ref mut e) in ret.iter_mut().rev() {
                let end = match end {
                    None => *end.insert(c),
                    Some(_) if Some(c + 1) != last => *end.insert(c),
                    Some(end) => end,
                };
                if *e == 0 {
                    *e = end;
                } else {
                    break;
                }
                last = Some(c)
            }

            if ret.len() == last_len {
                break Ok(ret);
            }
        }
    }

    pub async fn free(&self, chain_start: u32) -> Result<u32, Error> {
        self.cluster_chain(chain_start)
            .try_fold(0, |acc, cluster| async move {
                self.set(cluster, FatEntry::Free).await?;
                Ok(acc + 1)
            })
            .await
    }

    pub async fn truncate(&self, chain_start: u32) -> Result<u32, Error> {
        self.set(chain_start, FatEntry::End).await?;
        match self.iter_next(chain_start).await? {
            Some(next) => self.free(next).await,
            None => Ok(0),
        }
    }
}

const BATCH_LEN: usize = 64;
