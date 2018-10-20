use crate::platform::queue as sys_queue;

use std::{io};
use std::task::{LocalWaker, Poll};
use std::os::unix::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Filter(sys_queue::Filter);

impl Filter {
    pub const READ: Filter = Filter(sys_queue::Filter::READ);
    pub const WRITE: Filter = Filter(sys_queue::Filter::WRITE);
}

/**
 * The registration of a file descriptor with an [EventQueue](crate::queue::EventQueue).
 * 
 * An individual Registration should only be used with one [Future](std::future::Future) at a time for each
 * event filter. If multiple parties require notification of the same event type at the same time, a clone
 * should be made.
 */
#[derive(Clone, Debug)]
pub struct Registration(sys_queue::Registration);

pub trait RegistrarExt {
    /**
     * Registers a file descriptor with the [EventQueue](crate::queue::EventQueue).
     * 
     * This function may only be called once for each [EventQueue](crate::queue::EventQueue), file descriptor combination. If
     * multiple listeners to the state of a file descriptor are desired, they must be make from clones of the same [Registration](Registration)
     * instance.
     */
    fn register_fd(&self, source: RawFd) -> io::Result<Registration>;
}

impl RegistrarExt for crate::queue::Registrar {
    fn register_fd(&self, source: RawFd) -> io::Result<Registration> {
        Ok(Registration(self.inner.register_fd(source)?))
    }
}

impl Registration {
    /**
     * Determines whether a file descriptor is ready for a particular kind of operation, registering
     * a [LocalWaker](std::task::LocalWaker) to receive a signal once it is ready if it is not.
     * 
     * Note that this function does not guarantee that the file descriptor is in fact ready. It will only 
     * function properly if the caller attempts to perform the operation after receiving a ready
     * state, calling [clear_ready](Registration::clear_ready) if [WouldBlock](std::io::ErrorKind::WouldBlock) is
     * received.
     * 
     * Calling [poll_ready](Registration::poll_ready) or [clear_ready](Registration::clear_ready) with a different [LocalWaker](std::task::LocalWaker)
     * may cause the previously registered waker for this clone of [Registration](Registration) to be unregistered. A particular clone of the Registration 
     * should always be used with only one [Future](std::future::Future).
     */
    pub fn poll_ready(&mut self, filter: Filter, waker: &LocalWaker) -> Poll<io::Result<()>> {
        self.0.poll_ready(filter.0, waker)
    }

    /**
     * Called to notify the [EventQueue](crate::queue::EventQueue) that a [WouldBlock](std::io::ErrorKind::WouldBlock) was received and the
     * file descriptor must be scheduled for notifications of a state update via the queue. Also registers the
     * given [LocalWaker](std::task::LocalWaker) to be notified once the state changes.
     * 
     * Calling [poll_ready](Registration::poll_ready) or [clear_ready](Registration::clear_ready) with a different [LocalWaker](std::task::LocalWaker)
     * may cause the previously registered waker for this clone of [Registration](Registration) to be unregistered. A particular clone of the Registration 
     * should always be used with only one [Future](std::future::Future).
     */
    pub fn clear_ready(&mut self, filter: Filter, waker: &LocalWaker) -> io::Result<()> {
        self.0.clear_ready(filter.0, waker)
    }
}