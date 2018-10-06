use std::{io};
use std::time::Duration;

/// A construct capable of receiving events dispatched by the OS.
pub struct EventQueue {
    pub(crate) inner: crate::platform::queue::EventQueue,
}

impl EventQueue {
    pub fn new() -> io::Result<EventQueue> {
        Ok(EventQueue {
            inner: crate::platform::queue::EventQueue::new()?,
        })
    }

    pub fn turn(&self, max_wait: Option<Duration>, max_events: Option<usize>) -> io::Result<usize> {
        self.inner.turn(max_wait, max_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_queue_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<EventQueue>();
    }

    #[test]
    fn create_event_queue() {
        let _queue = EventQueue::new().unwrap();
    }

    #[test]
    fn turn_empty_queue() {
        let queue = EventQueue::new().unwrap();
        let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
        assert_eq!(0, nevents);
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use crate::os::unix::*;

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::task::{self, Poll, LocalWaker, Wake};
        use std::io::{Read, Write};
        use std::fs::{File};
        use std::os::unix::prelude::*;

        use nix::unistd::pipe;
        use libc;

        #[test]
        fn registration_is_send_and_sync() {
            fn is_send_and_sync<T: Send + Sync>() {}
            is_send_and_sync::<Registration>();
        }

        #[test]
        fn pipe_readable() {
            let queue = EventQueue::new().unwrap();
            let (rx, tx) = pipe().unwrap();
            let mut rx = unsafe { File::from_raw_fd(rx) };
            let mut tx = unsafe { File::from_raw_fd(tx) };
            let flags = unsafe { libc::fcntl(rx.as_raw_fd(), libc::F_GETFL, 0) };
            unsafe { libc::fcntl(rx.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK); }
            let mut registration = queue.register_fd(rx.as_raw_fd()).unwrap();

            let waker = Arc::new(CountWaker {
                count: AtomicUsize::new(0),
            });
            let local_waker = task::local_waker_from_nonlocal(waker.clone());
            // poll_ready should return ready since we haven't called clear_ready yet
            assert_eq!(Poll::Ready(()), registration.poll_ready(Filter::READ, &local_waker).unwrap());
            let mut buffer = [0u8; 32];
            match rx.read(&mut buffer) {
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {},
                res => panic!("unexpected result {:?}", res),
            }
            // Notify queue fd isn't ready
            registration.clear_ready(Filter::READ, &local_waker).unwrap();

            // Turn shouldn't return anything since the fd isn't ready
            let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
            assert_eq!(0, nevents);

            tx.write_all(b"hello!").unwrap();
            let nevents = queue.turn(Some(Duration::new(5, 0)), None).unwrap();
            assert_eq!(1, nevents);
            assert_eq!(1, waker.count.load(Ordering::SeqCst));
        }

        struct CountWaker {
            count: AtomicUsize,
        }

        impl Wake for CountWaker {
            fn wake(arc_self: &Arc<Self>) {
                arc_self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}