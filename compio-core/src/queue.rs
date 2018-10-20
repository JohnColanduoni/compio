use std::{io};
use std::time::Duration;

/// A construct capable of receiving events dispatched by the OS.
pub struct EventQueue {
    pub(crate) inner: crate::platform::queue::EventQueue,
}

#[derive(Clone, Debug)]
pub struct Registrar {
    pub(crate) inner: crate::platform::queue::Registrar,
}

pub struct UserEvent {
    pub(crate) inner: crate::platform::queue::UserEvent,
}

pub type UserEventHandler = Box<Fn(usize) + Send + Sync>;

impl EventQueue {
    pub fn new() -> io::Result<EventQueue> {
        Ok(EventQueue {
            inner: crate::platform::queue::EventQueue::new()?,
        })
    }

    pub fn turn(&self, max_wait: Option<Duration>, max_events: Option<usize>) -> io::Result<usize> {
        self.inner.turn(max_wait, max_events)
    }

    pub fn add_user_event(&self, handler: UserEventHandler) -> io::Result<UserEvent> {
        let inner = self.inner.add_user_event(handler)?;
        Ok(UserEvent { inner })
    }

    pub fn registrar(&self) -> Registrar {
        Registrar { inner: self.inner.registrar() }
    }
}

impl UserEvent {
    pub fn trigger(&self, data: usize) -> io::Result<()> {
        self.inner.trigger(data)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn event_queue_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<EventQueue>();
    }

    #[test]
    fn user_event_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<UserEvent>();
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

    #[test]
    fn simple_user_event() {
        let counter = Arc::new(AtomicUsize::new(0));
        let queue = EventQueue::new().unwrap();
        let user_event = queue.add_user_event(Box::new({
            let counter = counter.clone();
            move |data| {
                counter.swap(data, Ordering::SeqCst);
            }
        })).unwrap();
        // Shouldn't have any events prior to trigger
        let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
        assert_eq!(0, nevents);

        // Trigger event
        user_event.trigger(42).unwrap();
        let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
        assert_eq!(1, nevents);
        assert_eq!(42, counter.load(Ordering::SeqCst));

        // Shouldn't have any events after receiving instance
        let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
        assert_eq!(0, nevents);

        // Trigger event again
        user_event.trigger(62).unwrap();
        let nevents = queue.turn(Some(Duration::new(0, 0)), None).unwrap();
        assert_eq!(1, nevents);
        assert_eq!(62, counter.load(Ordering::SeqCst));

        // Shouldn't have any events after receiving instance
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
            match registration.poll_ready(Filter::READ, &local_waker) {
                Poll::Ready(Ok(())) => {},
                res => panic!("unexpected result {:?}", res),
            }
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