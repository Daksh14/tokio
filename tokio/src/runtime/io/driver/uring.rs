use io_uring::{squeue::Entry, IoUring, Probe};
use mio::unix::SourceFd;

use crate::io::Interest;
use crate::loom::sync::atomic::AtomicUsize;
use crate::loom::sync::Mutex;
use crate::runtime::driver::op::Lifecycle;

use super::{Handle, TOKEN_WAKEUP};

use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::{io, mem, task::Waker};

const DEFAULT_RING_SIZE: u32 = 256;

/// Tasks push entries straight into the submission queue under `sq`, which is
/// held only to copy an entry in and publish the tail. Everything else belongs
/// to the driver: it is the only caller of `io_uring_enter` and the only
/// consumer of the completion queue, and each op's result goes back through
/// its own [`Lifecycle`].
pub(crate) struct UringContext {
    uring: IoUring,
    /// Guards every submission queue view.
    sq: Mutex<SqState>,
    /// Entries pushed onto the submission queue whose CQE hasn't arrived yet.
    in_flight: AtomicUsize,
}

struct SqState {
    /// Whether the driver has been woken for entries pushed since its last submit.
    notified: bool,
}

impl UringContext {
    /// Perform the `io_uring_setup` system call.
    ///
    /// If the machine doesn't support io_uring, then this will return an
    /// `ENOSYS` error.
    fn new(probe: &mut Probe) -> io::Result<Self> {
        let uring = IoUring::new(DEFAULT_RING_SIZE)?;

        match uring.submitter().register_probe(probe) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                // The kernel does not support IORING_REGISTER_PROBE.
                return Err(io::Error::from_raw_os_error(libc::ENOSYS));
            }
            Err(e) => return Err(e),
        }

        Ok(Self {
            uring,
            sq: Mutex::new(SqState { notified: false }),
            in_flight: AtomicUsize::new(0),
        })
    }

    /// Returns whether the submission queue holds entries the kernel hasn't
    /// consumed yet.
    fn has_pending_submissions(&self) -> bool {
        let _state = self.sq.lock();
        // SAFETY: submission queue views are only created while `sq` is held.
        !unsafe { self.uring.submission_shared() }.is_empty()
    }

    fn cq_overflow(&self) -> bool {
        let _state = self.sq.lock();
        // SAFETY: submission queue views are only created while `sq` is held.
        unsafe { self.uring.submission_shared() }.cq_overflow()
    }

    /// # Safety
    ///
    /// Only the driver may call this: it is the only consumer of the
    /// completion queue.
    unsafe fn dispatch_completions(&self) {
        // SAFETY: the caller is the only consumer of the completion queue.
        let cq = unsafe { self.uring.completion_shared() };

        for cqe in cq {
            // SAFETY: `register_op` stored an `Arc::into_raw` pointer in
            // `user_data`, and the kernel posts exactly one CQE per entry.
            let lifecycle = unsafe { Arc::from_raw(cqe.user_data() as *const Lifecycle) };
            // Account for the CQE before waking: a panicking waker must not
            // leave `in_flight` counting an op whose CQE is already consumed.
            self.in_flight.fetch_sub(1, Relaxed);
            lifecycle.complete(cqe.result());
        }

        // `cq`'s drop gets called here (also on unwind), updating the latest head pointer
    }

    /// Submits every entry published so far.
    ///
    /// No lock is needed: `Submitter::submit` only reads the submission queue
    /// head and tail atomically, and the kernel takes whatever has been
    /// published.
    ///
    /// # Safety
    ///
    /// Only the driver may call this.
    unsafe fn submit(&self) -> io::Result<()> {
        loop {
            // Errors from io_uring_enter: https://man7.org/linux/man-pages/man2/io_uring_enter.2.html#ERRORS
            match self.uring.submit() {
                Ok(_) => return Ok(()),
                // If the completion queue is overflowing, we dispatch completions and try again.
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    // SAFETY: the caller is the driver.
                    unsafe { self.dispatch_completions() };
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => {}
                // For other errors, we currently return the error as is.
                Err(e) => return Err(e),
            }
        }
    }
}

/// Drop the driver, waiting for in-progress ops to terminate.
impl Drop for UringContext {
    fn drop(&mut self) {
        // Every `Op` holds a runtime handle, so by now all ops still in flight
        // were cancelled. Wait for their CQEs so the kernel is done with their
        // resources before they are freed. This also submits anything still
        // sitting in the submission queue.
        while self.in_flight.load(Relaxed) > 0 {
            self.uring
                .submit_and_wait(1)
                .expect("Internal error when dropping driver");

            for cqe in self.uring.completion() {
                // SAFETY: as in `dispatch_completions`.
                let lifecycle = unsafe { Arc::from_raw(cqe.user_data() as *const Lifecycle) };
                self.in_flight.fetch_sub(1, Relaxed);
                lifecycle.complete(cqe.result());
            }
        }
    }
}

impl Handle {
    fn add_uring_source(&self, uringfd: RawFd) -> io::Result<()> {
        let mut source = SourceFd(&uringfd);
        self.registry
            .register(&mut source, TOKEN_WAKEUP, Interest::READABLE.to_mio())
    }

    /// Returns `true` if io_uring has already been initialized and the given
    /// opcode is supported. Returns `false` if io_uring hasn't been
    /// initialized yet or is unsupported. Unlike `check_and_init`, this
    /// doesn't attempt initialization.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn is_uring_ready(&self, opcode: u8) -> bool {
        self.uring_probe
            .get()
            .and_then(|opt| opt.as_ref())
            .is_some_and(|probe| probe.is_supported(opcode))
    }

    /// Returns `true` if the io_uring probe has already been attempted
    /// (regardless of whether io_uring is supported). Returns `false` if
    /// no probe has been attempted yet.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn is_uring_probed(&self) -> bool {
        self.uring_probe.get().is_some()
    }

    /// Check if the io_uring context is initialized. If not, it will try to initialize it.
    /// Then, check if the provided opcode is supported.
    ///
    /// If both the context initialization succeeds and the opcode is supported,
    /// this returns `Ok(true)`.
    /// If either io_uring is unsupported or the opcode is unsupported,
    /// this returns `Ok(false)`.
    /// An error is returned if an io_uring syscall returns an unexpected error value.
    ///
    /// TODO: This would like to be a synchronous function,
    /// but we require `OnceLock::get_or_try_init`.
    /// <https://github.com/rust-lang/rust/issues/109737>
    pub(crate) async fn check_and_init(&self, opcode: u8) -> io::Result<bool> {
        let probe = self
            .uring_probe
            .get_or_try_init(|| async {
                let mut probe = Probe::new();
                match self.try_init(&mut probe) {
                    Ok(()) => Ok(Some(probe)),
                    // If the system doesn't support io_uring, we set the probe to `None`.
                    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => Ok(None),
                    // If we get EPERM, io-uring syscalls may be blocked (for example, by seccomp).
                    // In this case, we try to fall back to spawn_blocking for this and future operations.
                    // See also: https://github.com/tokio-rs/tokio/issues/7691
                    Err(e) if e.raw_os_error() == Some(libc::EPERM) => Ok(None),
                    // For other system errors, we just return it.
                    Err(e) => Err(e),
                }
            })
            .await?;

        Ok(probe
            .as_ref()
            .is_some_and(|probe| probe.is_supported(opcode)))
    }

    /// Initialize the io_uring context.
    fn try_init(&self, probe: &mut Probe) -> io::Result<()> {
        let ctx = UringContext::new(probe)?;
        self.add_uring_source(ctx.uring.as_raw_fd())?;
        // `check_and_init` runs this at most once at a time, and only until it
        // succeeds, so the cell is still empty.
        let _ = self.uring_context.set(ctx);

        Ok(())
    }

    /// Push an operation onto the io_uring submission queue. The driver
    /// submits it on its next turn.
    ///
    /// Hands the entry back if the submission queue is full; the caller should
    /// retry once the driver has run.
    ///
    /// # Safety
    ///
    /// Callers must ensure that parameters of the entry (such as buffer) are valid and will
    /// be valid for the entire duration of the operation, otherwise it may cause memory problems.
    pub(crate) unsafe fn register_op(
        &self,
        entry: Entry,
        waker: &Waker,
    ) -> Result<Arc<Lifecycle>, Entry> {
        let ctx = self.uring_context.get().expect("io_uring not initialized");

        let lifecycle = Arc::new(Lifecycle::new(waker));
        // The kernel's reference, reclaimed when the CQE arrives.
        let ptr = Arc::into_raw(lifecycle.clone());
        let entry = entry.user_data(ptr as u64);

        {
            let mut state = ctx.sq.lock();
            // SAFETY: submission queue views are only created while `sq` is held.
            let mut sq = unsafe { ctx.uring.submission_shared() };

            // SAFETY: entry is valid for the entire duration of the operation
            if unsafe { sq.push(&entry) }.is_err() {
                // The submission queue is full: take the kernel's reference back.
                // SAFETY: `ptr` came from `Arc::into_raw` above and the kernel never saw it.
                drop(unsafe { Arc::from_raw(ptr) });
                return Err(entry);
            }

            // Counted before the tail is published, so the CQE can't come first.
            ctx.in_flight.fetch_add(1, Relaxed);

            // Publishes the new tail while `sq` is still held.
            drop(sq);

            if mem::replace(&mut state.notified, true) {
                return Ok(lifecycle);
            }
        }

        // First push since the driver's last submit.
        self.unpark();

        Ok(lifecycle)
    }

    /// Submits pushed entries and dispatches completions.
    ///
    /// # Safety
    ///
    /// Only `Driver::turn` may call this: the driver is the only caller of
    /// `io_uring_enter` and the only consumer of the completion queue.
    pub(super) unsafe fn turn_uring(&self) {
        let Some(ctx) = self.uring_context.get() else {
            // Uring is not initialized yet.
            return;
        };

        // SAFETY: the caller is the driver.
        unsafe {
            // Free up completion queue space before submitting more.
            ctx.dispatch_completions();

            // Entries pushed from here on wake the driver again.
            mem::take(&mut ctx.sq.lock().notified);

            if ctx.has_pending_submissions() {
                match ctx.submit() {
                    Ok(()) => {}
                    // The kernel is out of resources: the entries stay queued.
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {}
                    Err(e) => panic!("unexpected error when submitting io_uring entries: {e}"),
                }

                // Entries that completed inline during submission.
                ctx.dispatch_completions();

                // Left behind by `EAGAIN` or a partial submit: come back next turn.
                if ctx.has_pending_submissions() && !mem::replace(&mut ctx.sq.lock().notified, true)
                {
                    self.unpark();
                }
            }

            // There might be some cases where the CQ overflows, so we need to flush
            // the remaining buffered CQEs.
            while ctx.cq_overflow() {
                ctx.submit()
                    .expect("failed to flush io_uring completion queue overflow");
                ctx.dispatch_completions();
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
