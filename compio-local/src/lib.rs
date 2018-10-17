#![feature(futures_api, pin)]

use std::{io};
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::future::Future;
use std::task::{self, Wake, Poll};

use compio_core::queue::{EventQueue, UserEvent, Registrar};
use scoped_tls::scoped_thread_local;
use pin_utils::pin_mut;
use futures_core::{
    future::LocalFutureObj,
    stream::Stream,
};
use futures_util::stream::FuturesUnordered;

pub struct LocalExecutor {
    shared: Arc<_LocalExecutor>,
    spanwed_futures: FuturesUnordered<LocalFutureObj<'static, ()>>,
}

struct _LocalExecutor {
    queue: EventQueue,
    // Incremented every time a Future being polled by this executor is awakened. A poll pass is not complete
    // until it finishes without this number changing.
    // TODO: padding for false sharing?
    awake_seq: AtomicUsize,
    remote_wake: UserEvent,
}

impl LocalExecutor {
    pub fn new() -> io::Result<LocalExecutor> {
        let queue = EventQueue::new()?;
        Self::with_event_queue(queue)
    }

    pub fn with_event_queue(queue: EventQueue) -> io::Result<LocalExecutor> {
        let remote_wake = queue.add_user_event(Box::new(|_data| {
            // This event doesn't need to do anything, just trigger an event that will
            // interrupt the event queue
        }))?;

        let shared = Arc::new(_LocalExecutor {
            queue,
            awake_seq: AtomicUsize::new(0),
            remote_wake,
        });

        Ok(LocalExecutor {
            shared,
            spanwed_futures: FuturesUnordered::new(),
        })
    }

    pub fn registrar(&self) -> io::Result<Registrar> {
        self.shared.queue.registrar()
    }

    pub fn queue(&mut self) -> &EventQueue {
        &self.shared.queue
    }

    pub fn block_on<F>(&mut self, future: F) -> F::Output where
        F: Future,
    {
        pin_mut!(future);
        let waker = unsafe { task::local_waker(self.shared.clone()) };

        let executor_id = _LocalExecutor::id(&self.shared);
        CURRENT_LOCAL_EXECUTOR_ID.set(&executor_id, move || {
            let mut init_seq = self.shared.awake_seq.load(Ordering::SeqCst);
            'outer: loop {
                match Pin::new(&mut future).poll(&waker) {
                    Poll::Ready(x) => return x,
                    Poll::Pending => {},
                }
                // Poll the spawned futures until there are either none left or they are all waiting
                // to be awoken
                loop {
                    match Pin::new(&mut self.spanwed_futures).poll_next(&waker) {
                        Poll::Ready(Some(())) => {},
                        Poll::Ready(None) => break,
                        Poll::Pending => break,
                    }
                }

                // If we made it through the polling pass without the awake sequence number changing, reset it to zero. If not, repeat the
                // pass with the new initial sequence value.
                match self.shared.awake_seq.compare_exchange(init_seq, 0, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => {},
                    Err(new_seq) => {
                        init_seq = new_seq;
                        continue;
                    }
                }

                'turn: loop {
                    self.shared.queue.turn(None, None).expect("failed to turn EventQueue");
                    init_seq = self.shared.awake_seq.load(Ordering::SeqCst);
                    if init_seq != 0 {
                        continue 'outer;
                    }
                }
            }
        })
    }

    pub fn spawn<F>(&mut self, future: F) where
        F: Future<Output = ()> + 'static
    {
        self.spawn_obj(LocalFutureObj::new(Box::new(future)));
    }

    pub fn spawn_obj(&mut self, future: LocalFutureObj<'static, ()>) {
        self.spanwed_futures.push(future);
    }
}

scoped_thread_local! {
    static CURRENT_LOCAL_EXECUTOR_ID: usize
}

impl _LocalExecutor {
    pub fn id(arc_self: &Arc<Self>) -> usize {
        &**arc_self as *const _LocalExecutor as usize
    }
}

impl Wake for _LocalExecutor {
    fn wake(arc_self: &Arc<Self>) {
        // Avoid triggering remote wakeup if we're already in a turn pass
        let needs_queue_event = if CURRENT_LOCAL_EXECUTOR_ID.is_set() {
            CURRENT_LOCAL_EXECUTOR_ID.with(|&current_id| current_id != _LocalExecutor::id(arc_self))
        } else {
            true
        };
        arc_self.awake_seq.fetch_add(1, Ordering::SeqCst);
        if needs_queue_event {
            arc_self.remote_wake.trigger(0).expect("failed to trigger wake of event queue");
        }
    }
}

