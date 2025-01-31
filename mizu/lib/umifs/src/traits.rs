use alloc::{boxed::Box, sync::Arc};

use arsc_rs::Arsc;
use async_trait::async_trait;
use ksc_core::Error;
use ktime_core::Instant;
pub use umio::{IntoAny, IntoAnyExt, Io, IoExt, ToIo};

use crate::{
    path::Path,
    types::{DirEntry, FsStat, Metadata, OpenOptions, Permissions},
};

#[async_trait]
pub trait FileSystem: IntoAny + Send + Sync + 'static {
    async fn root_dir(self: Arsc<Self>) -> Result<Arc<dyn Entry>, Error>;

    async fn flush(&self) -> Result<(), Error>;

    async fn stat(&self) -> FsStat;
}

#[async_trait]
pub trait Entry: IntoAny + Send + ToIo + Sync + 'static {
    async fn open(
        self: Arc<Self>,
        path: &Path,
        options: OpenOptions,
        perm: Permissions,
    ) -> Result<(Arc<dyn Entry>, bool), Error>;

    async fn metadata(&self) -> Metadata;

    async fn set_times(&self, c: Option<Instant>, m: Option<Instant>, a: Option<Instant>) {
        let _ = (c, m, a);
    }

    fn to_dir(self: Arc<Self>) -> Option<Arc<dyn Directory>> {
        None
    }

    fn to_dir_mut(self: Arc<Self>) -> Option<Arc<dyn DirectoryMut>> {
        None
    }
}

pub trait File: Entry + Io {}
impl<T: Entry + Io + ?Sized> File for T {}

#[async_trait]
pub trait Directory: Entry {
    async fn next_dirent(&self, last: Option<&DirEntry>) -> Result<Option<DirEntry>, Error>;
}

#[async_trait]
pub trait DirectoryMut: Directory {
    async fn rename(
        self: Arc<Self>,
        src_path: &Path,
        dst_parent: Arc<dyn DirectoryMut>,
        dst_path: &Path,
    ) -> Result<(), Error>;

    async fn link(
        self: Arc<Self>,
        src_path: &Path,
        dst_parent: Arc<dyn DirectoryMut>,
        dst_path: &Path,
    ) -> Result<(), Error>;

    async fn unlink(&self, path: &Path, expect_dir: Option<bool>) -> Result<(), Error>;
}
