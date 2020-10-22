use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    sync::atomic::AtomicBool,
    sync::atomic::Ordering,
    task::{Context, Poll, Waker},
};

use futures_util::task::AtomicWaker;

use super::{sys::io_uring_sqe, IoUring};

pub struct Operation<'a> {
    io_uring: &'a IoUring,
    submit_state: SubmitState,
    shared: UnsafeCell<Shared>,
}

unsafe impl<'a> Send for Operation<'a> {}

enum SubmitState {
    Unsubmitted { sqe: io_uring_sqe },
    Submitted { op_id: u64 },
}

// This portion of the `Operation` is shared with the completer, and a pointer to it is held in the
// operation table of the `IoUring` structure. The ID mapping in that table is what is given to the
// `user_data` field of the submission for this operation.
//
// This may seem unsafe, but we avoid that by only sharing this pointer after `poll` is called,
// which requires that the `Operation` be pinned. Let's analyze the lifetime situations we can
// find ourselves in:
//
//  1. `Operation` stays actively referenced for the duration of the `io_uring` operation: no problem, since this
//     structure is pinned the address will be stable for the duration it is referenced by the `user_data` field.
//  2. `Operation` is dropped: we inform the `io_uring` that the operation has been cancelled. It removes the
//      mapping of the operation ID to the pointer to the `Shared` structure, which means we can no longer inadverdently
//      write to this structure after it is freed.
//  3. `Operation` is leaked (which is allowed in safe code). First note that it is not possible to leak an `Operation` that
//      has been started via a direct `mem::forget`, since that would violate the `Pin` contract by moving the structure. So
//      the `Operation` is in one of:
//          a. In a pinned structure on the heap (`Box`, `Arc`, etc.): the container must be leaked as well (because it's in a `Pin`) so
//             the completer can write there safely (though nobody will ever bother to look).
//          b. On the stack via a `pin_mut`: the destructor will be run when the function is left or unwinding, so a leak is not possible here
//             unless the thread is forcibly terminated separate from the process lifetime. Doing so is already unsound, as safe code will allow you to
//             store a pointer to a thread's stack in a structure on the heap visible to other threads.
//          c. On the suspended "stack" of a `async fn`: the same pinning analysis from the previous points applies to the containing future in question.
//
// Note that this does not cover the lifetimes of any buffers that the underlying `io_uring` operation
// may access. Those are the responsibility of the embedders of this type, and not part of its contract (hence the unsafe `new` method).
struct Shared {
    done: AtomicBool,
    // Before `done` is set, owned by completer. After `done` is set, owned by
    // holder of the reference to the `Operation` struct.
    result: UnsafeCell<Option<OperationResult>>,
    waker: AtomicWaker,
}

impl<'a> Drop for Operation<'a> {
    fn drop(&mut self) {
        if let SubmitState::Submitted { op_id } = self.submit_state {
            self.io_uring.cancel_op(op_id);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OperationResult {
    pub res: i32,
    pub flags: u32,
}

impl<'a> Operation<'a> {
    pub(super) unsafe fn new(io_uring: &'a IoUring, sqe: io_uring_sqe) -> Self {
        Operation {
            io_uring,
            submit_state: SubmitState::Unsubmitted { sqe },
            shared: UnsafeCell::new(Shared {
                done: AtomicBool::new(false),
                result: UnsafeCell::new(None),
                waker: AtomicWaker::new(),
            }),
        }
    }
}

impl<'a> Future for Operation<'a> {
    type Output = OperationResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.submit_state {
            SubmitState::Unsubmitted { sqe } => unimplemented!(),
            SubmitState::Submitted { .. } => {
                let shared = unsafe { &*(self.shared.get()) };

                // TODO: consider holding a copy of the `Waker` outside of the `AtomicWaker` to determine if there is no need to
                // write to the `AtomicWaker` (which will be the most common case for a duplicated poll). In addition, this would
                // allow using `Acquire` ordering on the following load from `done` in that case (it still needs to be `SeqCst` in the
                // event the waker needs to be set however).
                //
                // TODO: It also may make sense to do our own atomic waker management here, since we don't need to support concurrent `register` (I *think* we
                // can do a `Relaxed` load since we're the only writer to the variable).
                shared.waker.register(cx.waker());

                // Check if done
                //
                // The sequentially consistent ordering here is needed because we need an ordering relative to the atomic operations
                // inside `AtomicWaker::register`.
                if shared.done.load(Ordering::SeqCst) {
                    // Once we load a `true` value from the `done` flag, we have ownership of `result`
                    match unsafe { (*shared.result.get()).take() } {
                        Some(res) => Poll::Ready(res),
                        None => panic!("polled a ready future multiple times"),
                    }
                } else {
                    Poll::Pending
                }
            }
        }
    }
}
