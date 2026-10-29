use super::super::{Driver, Handle};
use super::DEFAULT_RING_SIZE;

use io_uring::{opcode, Probe};
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

// Builds a driver whose io_uring context is initialized, or returns `None` when
// io_uring is unavailable on the host.
fn ready_handle() -> Option<(Driver, Handle)> {
    let (driver, handle) = Driver::new(1024, None).ok()?;

    let mut probe = Probe::new();
    handle.try_init(&mut probe).ok()?;
    handle.uring_probe.set(Some(probe)).unwrap();

    Some((driver, handle))
}

fn in_flight(handle: &Handle) -> usize {
    handle.uring_context.get().unwrap().in_flight.load(Relaxed)
}

// Registering only pushes the entry; the driver submits it and dispatches the
// completion on its next turn.
#[test]
#[cfg_attr(miri, ignore)]
fn op_completes_after_driver_turn() {
    let Some((mut driver, handle)) = ready_handle() else {
        return;
    };

    let mut cx = Context::from_waker(Waker::noop());

    // SAFETY: the no-op entry does not reference any buffer.
    let lifecycle = unsafe { handle.register_op(opcode::Nop::new().build(), Waker::noop()) }
        .expect("the submission queue has room");
    assert!(lifecycle.poll(&mut cx).is_pending());
    assert_eq!(in_flight(&handle), 1);

    let mut res = Poll::Pending;
    for _ in 0..100 {
        driver.turn(&handle, Some(Duration::from_millis(10)));
        res = lifecycle.poll(&mut cx);
        if res.is_ready() {
            break;
        }
    }

    assert_eq!(res, Poll::Ready(0));
    assert_eq!(in_flight(&handle), 0);
}

// A full submission queue hands the entry back instead of blocking, and has
// room again once the driver has submitted.
#[test]
#[cfg_attr(miri, ignore)]
fn full_submission_queue_hands_entry_back() {
    let Some((mut driver, handle)) = ready_handle() else {
        return;
    };

    let mut ops = Vec::new();
    for _ in 0..DEFAULT_RING_SIZE {
        // SAFETY: the no-op entry does not reference any buffer.
        let op = unsafe { handle.register_op(opcode::Nop::new().build(), Waker::noop()) };
        ops.push(op.expect("the submission queue has room"));
    }

    // SAFETY: the no-op entry does not reference any buffer.
    let full = unsafe { handle.register_op(opcode::Nop::new().build(), Waker::noop()) };
    assert!(full.is_err());
    assert_eq!(in_flight(&handle), DEFAULT_RING_SIZE as usize);

    driver.turn(&handle, Some(Duration::ZERO));

    // SAFETY: the no-op entry does not reference any buffer.
    let retried = unsafe { handle.register_op(opcode::Nop::new().build(), Waker::noop()) };
    assert!(retried.is_ok());
}
