use crate::io::blocking::Buf;
use crate::io::uring::open::Open;
use crate::io::uring::read::Read;
use crate::io::uring::rename::Rename;
use crate::io::uring::utils::ArcFd;
use crate::io::uring::write::Write;

use crate::loom::cell::UnsafeCell;
use crate::loom::sync::atomic::{AtomicU32, AtomicU8};
use crate::runtime::Handle;
use crate::sync::AtomicWaker;

#[cfg(
    // libc::statx is only supported on these platforms
    // FIXME: Add musl target env when our minimum supported
    // rust version is 1.93. To clarify, statx support is
    // introduced to musl in 1.25 as mentioned officially here:
    // https://musl.libc.org/releases.html.
    // However, rustup target_env building for *-linux-musl
    // uses 1.25 musl on all *-linux-musl platforms starting
    // in 1.93 stable rust version.
    // https://blog.rust-lang.org/2025/12/05/Updating-musl-1.2.5/
    any(target_env = "gnu", target_os = "android")
)]
use crate::io::uring::statx::Statx;
use io_uring::squeue::Entry;
use std::future::Future;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

// This field isn't accessed directly, but it holds cancellation data,
// so `#[allow(dead_code)]` is needed.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum CancelData {
    Open(Open),
    Write(Write),
    Rename(Rename),
    ReadVec(Read<Vec<u8>, OwnedFd>),
    ReadBuf(Read<Buf, ArcFd>),
    #[cfg(
        // libc::statx is only supported on these platforms
        // FIXME: Add musl target env when our minimum supported
        // rust version is 1.93. To clarify, statx support is
        // introduced to musl in 1.25 as mentioned officially here:
        // https://musl.libc.org/releases.html.
        // However, rustup target_env building for *-linux-musl
        // uses 1.25 musl on all *-linux-musl platforms starting
        // in 1.93 stable rust version.
        // https://blog.rust-lang.org/2025/12/05/Updating-musl-1.2.5/
        any(target_env = "gnu", target_os = "android")
    )]
    Statx(Statx),
}

const PENDING: u8 = 0;
const COMPLETED: u8 = 1;
const CANCELLED: u8 = 2;

/// State an [`Op`] shares with the driver.
///
/// The driver holds a reference from when the entry is pushed onto the
/// submission queue until its CQE arrives; `user_data` is that reference.
pub(crate) struct Lifecycle {
    state: AtomicU8,
    /// The CQE result, valid once `state` is `COMPLETED`.
    result: AtomicU32,
    waker: AtomicWaker,
    /// The op's resources, handed over when the `Op` is dropped before completing.
    cancel_data: UnsafeCell<Option<CancelData>>,
}

// SAFETY: the `Op` writes `cancel_data` before publishing `CANCELLED`, and only
// the side that loses the race out of `PENDING` reads it afterwards.
unsafe impl Sync for Lifecycle {}

impl Lifecycle {
    pub(crate) fn new(waker: &Waker) -> Self {
        let lifecycle = Self {
            state: AtomicU8::new(PENDING),
            result: AtomicU32::new(0),
            waker: AtomicWaker::new(),
            cancel_data: UnsafeCell::new(None),
        };
        lifecycle.waker.register_by_ref(waker);
        lifecycle
    }

    /// Records the CQE result. Called by the driver.
    pub(crate) fn complete(&self, res: i32) {
        self.result.store(res as u32, Relaxed);
        match self
            .state
            .compare_exchange(PENDING, COMPLETED, AcqRel, Acquire)
        {
            Ok(_) => self.waker.wake(),
            // The `Op` is gone, so its resources are released here.
            Err(_) => self.release(res),
        }
    }

    /// Hands the op's resources over while it may still be in flight. Called
    /// when the `Op` is dropped.
    pub(crate) fn cancel(&self, data: CancelData) {
        // SAFETY: the driver reads `cancel_data` only after it sees `CANCELLED`.
        self.cancel_data
            .with_mut(|ptr| unsafe { *ptr = Some(data) });
        if self
            .state
            .compare_exchange(PENDING, CANCELLED, AcqRel, Acquire)
            .is_err()
        {
            // The CQE arrived but was never observed.
            self.release(self.result.load(Relaxed) as i32);
        }
    }

    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<i32> {
        if self.state.load(Acquire) != COMPLETED {
            self.waker.register_by_ref(cx.waker());
            // The CQE may have arrived before the new waker was registered.
            if self.state.load(Acquire) != COMPLETED {
                return Poll::Pending;
            }
        }
        Poll::Ready(self.result.load(Relaxed) as i32)
    }

    /// Drops the resources of a cancelled op once the kernel is done with them.
    fn release(&self, res: i32) {
        // SAFETY: both sides have left `PENDING`, so this is the only access.
        let data = self.cancel_data.with_mut(|ptr| unsafe { (*ptr).take() });
        if matches!(data, Some(CancelData::Open(_))) && res >= 0 {
            // SAFETY: a successful open CQE carries a new fd that nothing else owns.
            drop(unsafe { OwnedFd::from_raw_fd(res) });
        }
    }
}

pub(crate) enum State {
    Initialize(Option<Entry>),
    Polled(Arc<Lifecycle>),
    Complete,
}

pub(crate) struct Op<T: Cancellable> {
    // Handle to the runtime
    handle: Handle,
    // State of this Op
    state: State,
    // Per operation data.
    data: Option<T>,
}

impl<T: Cancellable> Op<T> {
    /// # Safety
    ///
    /// Callers must ensure that parameters of the entry (such as buffer) are valid and will
    /// be valid for the entire duration of the operation, otherwise it may cause memory problems.
    pub(crate) unsafe fn new(entry: Entry, data: T) -> Self {
        let handle = Handle::current();
        Self {
            handle,
            data: Some(data),
            state: State::Initialize(Some(entry)),
        }
    }
    pub(crate) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }
}

impl<T: Cancellable> Drop for Op<T> {
    fn drop(&mut self) {
        // A registered op that hasn't completed keeps its resources alive until
        // the kernel is done with them.
        if let State::Polled(lifecycle) = &self.state {
            let data = self.data.take().expect("Data should be present");
            lifecycle.cancel(data.cancel());
        }
    }
}

/// A single CQE result
pub(crate) struct CqeResult {
    pub(crate) result: io::Result<u32>,
}

impl From<i32> for CqeResult {
    fn from(res: i32) -> Self {
        let result = if res >= 0 {
            Ok(res as u32)
        } else {
            Err(io::Error::from_raw_os_error(-res))
        };
        CqeResult { result }
    }
}

/// A trait that converts a CQE result into a usable value for each operation.
pub(crate) trait Completable {
    type Output;
    fn complete(self, cqe: CqeResult) -> Self::Output;
}

/// Extracts the `CancelData` needed to safely cancel an in-flight io_uring operation.
pub(crate) trait Cancellable {
    fn cancel(self) -> CancelData;
}

impl<T: Cancellable> Unpin for Op<T> {}

impl<T: Cancellable + Completable + Send> Future for Op<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match &mut this.state {
            State::Initialize(entry_opt) => {
                let entry = entry_opt.take().expect("Entry must be present");
                let driver = this.handle.inner.driver().io();

                // SAFETY: entry is valid for the entire duration of the operation
                match unsafe { driver.register_op(entry, cx.waker()) } {
                    Ok(lifecycle) => this.state = State::Polled(lifecycle),
                    // The submission queue is full. Retry after the next park,
                    // which turns the driver.
                    Err(entry) => {
                        *entry_opt = Some(entry);
                        crate::runtime::context::defer(cx.waker());
                    }
                }

                Poll::Pending
            }

            State::Polled(lifecycle) => {
                let res = std::task::ready!(lifecycle.poll(cx));
                this.state = State::Complete;

                let data = this
                    .take_data()
                    .expect("Data must be present on completion");
                Poll::Ready(data.complete(res.into()))
            }

            State::Complete => {
                panic!("Future polled after completion");
            }
        }
    }
}
