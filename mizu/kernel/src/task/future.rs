use core::{
    future::Future,
    ops::ControlFlow::{Break, Continue},
    pin::Pin,
    task::{Context, Poll},
};

use arsc_rs::Arsc;
use co_trap::{FastResult, TrapFrame};
use kmem::Virt;
use ksc::{Scn, ENOSYS};
use pin_project::pin_project;
use riscv::register::{
    scause::{Exception, Scause, Trap},
    time,
};
use sygnal::{Sig, SigCode, SigInfo};

use super::TaskState;
use crate::{syscall::ScRet, task::signal::SIGRETURN_GUARD};

#[pin_project]
pub struct TaskFut<F> {
    virt: Pin<Arsc<Virt>>,
    #[pin]
    fut: F,
}

impl<F> TaskFut<F> {
    pub fn new(virt: Pin<Arsc<Virt>>, fut: F) -> Self {
        TaskFut { virt, fut }
    }
}

impl<F: Future> Future for TaskFut<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let clear = unsafe { self.virt.clone().load() };
        if let Some(clear) = clear {
            crate::executor().spawn(clear).detach();
        }
        self.project().fut.poll(cx)
    }
}

const TASK_GRAN: u64 = 20000;

pub async fn user_loop(mut ts: TaskState, mut tf: TrapFrame) {
    log::debug!("task {} startup, tf.a0 = {}", ts.task.tid, tf.gpr.tx.a[0]);

    let mut stat_time = time::read64();
    let mut sched_time = stat_time;
    let (code, sig) = 'life: loop {
        match ts.handle_signals(&mut tf).await {
            Ok(()) => {}
            Err((code, sig)) => break 'life (code, Some(sig)),
        }

        let sys = time::read64();
        ts.system_times += sys - stat_time;
        stat_time = sys;

        log::trace!(
            "task {} entering user cx, sepc = {:#x}",
            ts.task.tid,
            tf.sepc
        );
        let (scause, fr) = co_trap::yield_to_user(&mut tf);

        let usr = time::read64();
        ts.user_times += usr - stat_time;
        stat_time = usr;

        match fr {
            FastResult::Continue => {}
            FastResult::Pending => continue,
            FastResult::Break => break 'life (0, None),
            FastResult::Yield => unreachable!(),
        }

        match handle_scause(scause, &mut ts, &mut tf).await {
            Continue(Some(sig)) => ts.task.sig.push(sig),
            Continue(None) => {}
            Break(code) => break 'life (code, None),
        }

        let now = time::read64();
        if now - sched_time >= TASK_GRAN {
            sched_time = now;
            log::trace!("task {} yield", ts.task.tid);
            yield_now().await;
            log::trace!("task {} yielded", ts.task.tid);
        }
    };
    ts.cleanup(code, sig).await
}

async fn handle_scause(scause: Scause, ts: &mut TaskState, tf: &mut TrapFrame) -> ScRet {
    match scause.cause() {
        Trap::Interrupt(intr) => crate::trap::handle_intr(intr, "user task"),
        Trap::Exception(excep) => match excep {
            Exception::UserEnvCall => {
                let res = async {
                    let scn = tf.scn().ok_or(None)?;
                    if scn != Scn::WRITE {
                        log::info!(
                            "task {} syscall {scn:?}, sepc = {:#x}",
                            ts.task.tid,
                            tf.sepc
                        );
                    }
                    crate::syscall::SYSCALL
                        .handle(scn, (ts, tf))
                        .await
                        .ok_or(Some(scn))
                }
                .await;
                match res {
                    Ok(res) => return res,
                    Err(scn) => {
                        log::warn!("SYSCALL not implemented: {scn:?}");
                        tf.set_syscall_ret(ENOSYS.into_raw())
                    }
                }
            }
            Exception::InstructionPageFault
            | Exception::LoadPageFault
            | Exception::StorePageFault => {
                log::info!(
                    "task {} {excep:?} at {:#x}, address = {:#x}",
                    ts.task.tid,
                    tf.sepc,
                    tf.stval
                );
                if tf.stval == SIGRETURN_GUARD {
                    return TaskState::resume_from_signal(ts, tf).await;
                }

                let res = ts.virt.commit(tf.stval.into()).await;
                if let Err(err) = res {
                    log::error!("failing to commit pages at address {:#x}: {err}", tf.stval);
                    return Continue(Some(SigInfo {
                        sig: Sig::SIGSEGV,
                        code: SigCode::KERNEL as _,
                        fields: sygnal::SigFields::SigSys {
                            addr: tf.stval.into(),
                            num: 0,
                        },
                    }));
                }
            }
            _ => panic!(
                "task {} unhandled excep {excep:?} at {:#x}, stval = {:#x}",
                ts.task.tid, tf.sepc, tf.stval
            ),
        },
    }
    Continue(None)
}

pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

/// Future for the [`yield_now()`] function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}
