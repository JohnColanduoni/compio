#[cfg(target_os = "macos")]
pub mod mach;

use crate::queue::UserEventHandler;

use std::{io, mem, ptr, cmp, fmt, ops};
use std::collections::HashSet;
use std::sync::{
    Arc, Weak, Mutex,
    atomic::{Ordering, AtomicBool, AtomicUsize},
};
use std::time::Duration;
use std::os::raw::{c_int, c_long};
use std::task::{LocalWaker, Waker, Poll};
use std::os::unix::prelude::*;

use libc::{self, c_void};
use crossbeam::queue::SegQueue;

pub struct EventQueue(Arc<_EventQueue>);

struct _EventQueue {
    fd: RawFd,
}

impl Drop for _EventQueue {
    fn drop(&mut self) {
        // FIXME: remove extant Arc<_Registration>s from queue and drop
        unsafe { libc::close(self.fd); }
    }
}

#[derive(Clone)]
pub struct Registrar(Arc<_EventQueue>);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Filter(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FilterSet(u32);

pub struct Registration {
    inner: Arc<_Registration>,
    // Used to keep spurious poll calls from filling up the Waker queue in the
    // RegistrationState. The `usize` is a sequence number from the RegistrationState, and
    // the Waker is a clone of the currently subscribed waker for the `will_wake` check.
    wakers: FilterCollection<Option<(usize, Waker)>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u32)]
enum TokenKind {
    Registration,
    UserEvent,
}

#[repr(C)] // _Registration must start with TokenKind
struct _Registration {
    token_kind: TokenKind,
    // TODO: this pointer will be upgraded whenever a filter registration is made, which may cause a lot of fighting over the reference count
    // for this pointer. Maybe just warn when EventQueue is dropped and the Arc is not unique?
    queue: Weak<_EventQueue>,
    fd: RawFd,
    // TODO: consider padding to avoid false sharing?
    states: FilterCollection<RegistrationState>,
}

impl Drop for _Registration {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.upgrade() {
            for (filter, state) in self.states.iter() {
                unsafe {
                    let mut event = libc::epoll_event {
                        events: filter.0, // There is a bug in some old kernels that errors if this is 0 for EPOLL_CTL_DEL
                        u64: self as *const _Registration as usize as u64,
                    };
                    if libc::epoll_ctl(queue.fd, libc::EPOLL_CTL_DEL, state.dup_fd, &mut event) < 0 {
                        let err = io::Error::last_os_error();
                        error!("epoll_ctl to remove file descriptor {:?} failed: {}", state.dup_fd, err);
                    }
                }
            }
        }
    }
}

struct RegistrationState {
    // We duplicate the file descriptor so we can adjust the state for each event type independently. This reduces
    // contention and churn for bidirection file descriptors.
    dup_fd: RawFd,
    // When this value is even, the given state is "ready", an not ready when odd. The sequence number is used so 
    // it can be determined whether the current task is already subscribed to notifications.
    sequence: AtomicUsize,
    listeners: SegQueue<Waker>,
}

impl Drop for RegistrationState {
    fn drop(&mut self) {
        unsafe { libc::close(self.dup_fd); }
    }
}

pub struct UserEvent(Arc<_UserEvent>);

#[repr(C)] // _UserEvent must start with TokenKind
struct _UserEvent {
    token_kind: TokenKind,
    queue: Arc<_EventQueue>,
    event_fd: RawFd,
    handler: UserEventHandler,

    armed: AtomicBool,
    values: SegQueue<usize>,
}

impl Drop for _UserEvent {
    fn drop(&mut self) {
        unsafe { libc::close(self.event_fd); }
    }
}


impl EventQueue {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { try_libc!(fd: libc::epoll_create1(libc::EPOLL_CLOEXEC), "epoll_create1() failed: {}") };
        Ok(EventQueue(Arc::new(_EventQueue {
            fd,
        })))
    }

    pub fn registrar(&self) -> Registrar {
        Registrar(self.0.clone())
    }

    pub fn turn(&self, max_wait: Option<Duration>, max_events: Option<usize>) -> io::Result<usize> {
        unsafe {
            let mut events: [libc::epoll_event; 64] = mem::zeroed();

            let event_count = loop {
                let event_count = libc::epoll_wait(
                    self.0.fd,
                    events.as_mut_ptr(),
                    max_events.map(|x| cmp::min(x, events.len())).unwrap_or(events.len()) as c_int,
                    max_wait
                        .map(|x| x.as_secs().saturating_mul(1000).saturating_add(x.subsec_millis() as u64))
                        .map(|x| if x > libc::INT_MAX as u64 {
                            libc::INT_MAX
                        } else if x < 1 {
                            1 // If the value is less than one millisecond, ensure we wait one millisecond instead of waiting forever
                        } else {
                            x as c_int
                        })
                        .unwrap_or(0),
                );

                if event_count < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    error!("epoll_wait failed: {}", err);
                    return Err(err);
                } else {
                    break event_count;
                }
            };

            let events = &events[0..(event_count as usize)];

            for event in events.iter() {
                let filter = FilterSet(event.events);

                for event_type in filter.iter() {
                    // Only EPOLLIN and EPOLLOUT come with a positive ref count on the _Registration/_UserEvent
                    if event_type == Filter::READ || event_type == Filter::WRITE {
                        let token_kind = unsafe { *(event.u64 as usize as *const TokenKind) };
                        match token_kind {
                            TokenKind::Registration => {
                                let registration = Arc::from_raw(event.u64 as usize as *const _Registration);
                                let state = &registration.states[event_type];
                                let old_seq = state.sequence.fetch_add(1, Ordering::SeqCst);
                                assert!(old_seq % 2 == 1, "registration should only be present in EPOLLIN or EPOLLOUT event if it was most recently added to epoll");
                                while let Some(waker) = state.listeners.try_pop() {
                                    waker.wake();
                                }
                            },
                            TokenKind::UserEvent => {
                                let user_event = Arc::from_raw(event.u64 as usize as *const _UserEvent);
                                if cfg!(debug_assertions) {
                                    let old_armed = user_event.armed.swap(false, Ordering::SeqCst);
                                    assert!(old_armed, "UserEvent should only be present in EPOLLIN or EPOLLOUT event if it was most recently added to epoll");
                                } else {
                                    user_event.armed.store(false, Ordering::SeqCst);
                                }
                                loop {
                                    let mut count = [0u8; 8];
                                    if libc::read(user_event.event_fd, count.as_mut_ptr() as _, count.len()) < 0 {
                                        let err = io::Error::last_os_error();
                                        match err.kind() {
                                            io::ErrorKind::Interrupted => continue,
                                            io::ErrorKind::WouldBlock => {
                                                panic!("inconsistent state of eventfd backing UserEvent");
                                            },
                                            _ => return Err(err),
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                while let Some(value) = user_event.values.try_pop() {
                                    (user_event.handler)(value);
                                }
                            },
                        }
                    }
                }
            }

            Ok(events.len())
        }
    }


    pub fn add_user_event(&self, handler: UserEventHandler) -> io::Result<UserEvent> {
        let event_fd = unsafe { try_libc!(fd: libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK), "failed to create eventfd for UserEvent: {}") };

        let inner = Arc::new(_UserEvent {
            token_kind: TokenKind::UserEvent,
            queue: self.0.clone(),
            event_fd,
            handler,

            armed: AtomicBool::new(false),
            values: Default::default(),
        });

        unsafe {
            let mut event = libc::epoll_event {
                events: 0,
                u64: &*inner as *const _UserEvent as usize as u64,
            };
            try_libc!(libc::epoll_ctl(self.0.fd, libc::EPOLL_CTL_ADD, inner.event_fd, &mut event), "failed to add eventfd for UserEvent to epoll: {}");
        }

        Ok(UserEvent(inner))
    }
}

impl Registrar {
    pub fn register_fd(&self, source: RawFd) -> io::Result<Registration> {
        let states = FilterCollection::try_new(|_| {
            let dup_fd = unsafe { try_libc!(fd: libc::fcntl(source, libc::F_DUPFD_CLOEXEC), "failed to duplicate file descriptor for epoll registration: {}") };
            Ok(RegistrationState {
                dup_fd,
                sequence: AtomicUsize::new(0),
                listeners: Default::default(),
            })
        })?;

        let registration = Registration {
            inner: Arc::new(_Registration {
                token_kind: TokenKind::Registration,
                queue: Arc::downgrade(&self.0),
                fd: source,
                states,
            }),
            wakers: Default::default(),
        };

        for (_filter, state) in registration.inner.states.iter() {
            let mut event = libc::epoll_event {
                events: 0,
                u64: &*registration.inner as *const _Registration as usize as u64,
            };
            unsafe {
                try_libc!(libc::epoll_ctl(self.0.fd, libc::EPOLL_CTL_ADD, state.dup_fd, &mut event), "failed to add file descriptor {1:?} to epoll: {0}", state.dup_fd);
            }
        }

        Ok(registration)
    }
}

impl fmt::Debug for EventQueue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventQueue")
            .field("epoll", &format_args!("{:?}", self.0.fd))
            .finish()
    }
}

impl fmt::Debug for Registrar {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Registrar")
            .field("epoll", &format_args!("{:?}", self.0.fd))
            .finish()
    }
}

pub trait EventQueueExt {

}

impl Registration {
    pub fn poll_ready(&mut self, filter: Filter, waker: &LocalWaker) -> Poll<io::Result<()>> {
        let state = &self.inner.states[filter];
        let mut sequence = state.sequence.load(Ordering::SeqCst);
        loop {
            if sequence % 2 == 0 {
                self.wakers[filter] = None;
                return Poll::Ready(Ok(()));
            }

            if let Some((existing_sequence, existing_waker)) = &self.wakers[filter] {
                if *existing_sequence == sequence && waker.will_wake_nonlocal(existing_waker) {
                    return Poll::Pending;
                }
            }

            state.listeners.push(waker.clone().into());

            // Check state again, just in case there was a race
            match state.sequence.load(Ordering::SeqCst) {
                x if x == sequence => {
                    self.wakers[filter] = Some((sequence, waker.clone().into()));
                    return Poll::Pending;
                },
                new_seq => {
                    sequence = new_seq;
                    continue;
                },
            }
        }
    }

    pub fn clear_ready(&mut self, filter: Filter, waker: &LocalWaker) -> io::Result<()> {
        let state = &self.inner.states[filter];
        // If we're the first to set the state to not ready, it's our job to subscribe the fd to epoll for the desired
        // filter.
        let old_sequence = state.sequence.fetch_or(0x1, Ordering::SeqCst);
        if old_sequence % 2 == 0 {
            // Ensure we put our waker in the queue after we're sure we're on a not listening -> listening edge
            state.listeners.push(waker.clone().into());
            unsafe {

                let queue = if let Some(queue) = self.inner.queue.upgrade() { queue } else {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "a request was made to register a file descriptor to receive events on an EventQueue, but that EventQueue has been dropped"));
                };

                let ptr = Arc::into_raw(self.inner.clone());

                let mut event = libc::epoll_event {
                    events: filter.0 | libc::EPOLLET as u32 | libc::EPOLLONESHOT as u32,
                    u64: ptr as usize as u64,
                };
                try_libc!(libc::epoll_ctl(queue.fd, libc::EPOLL_CTL_MOD, state.dup_fd, &mut event), "epoll_ctl to rearm event {1:?} for file descriptor {2:?}: {0}", filter, state.dup_fd);
            }
        } else {
            state.listeners.push(waker.clone().into());
        }
        self.wakers[filter] = Some((old_sequence | 0x1, waker.clone().into()));

        Ok(())
    }
}

impl Clone for Registration {
    fn clone(&self) -> Self {
        Registration {
            inner: self.inner.clone(),
            wakers: Default::default(),
        }
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Registration")
            .field("fd", &self.inner.fd)
            .field("read_registered", &self.wakers[Filter::READ].is_some())
            .field("write_registered", &self.wakers[Filter::WRITE].is_some())
            .finish()
    }
}

impl Filter {
    pub const READ: Filter = Filter(libc::EPOLLIN as u32);
    pub const WRITE: Filter = Filter(libc::EPOLLOUT as u32);
}

impl FilterSet {
    fn iter(&self) -> impl Iterator<Item = Filter> {
        let value = self.0;
        FILTER_FLAGS.iter().filter_map(move |&(flag, _)| {
            if value & flag.0 != 0 {
                Some(flag)
            } else {
                None
            }
        })
    }
}

const FILTER_FLAGS: &'static [(Filter, &'static str)] = &[
    (Filter::READ, "READ"),
    (Filter::WRITE, "WRITE"),
];

impl fmt::Debug for Filter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Filter::READ => write!(f, "Filter::READ"),
            Filter::WRITE => write!(f, "Filter::WRITE"),
            _ => write!(f, "Filter({:#x?})", self.0),
        }
    }
}

impl fmt::Debug for FilterSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "FilterSet([")?;
        let mut first = true;
        let mut remaining_value = self.0;
        for (flag, name) in FILTER_FLAGS.iter() {
            if remaining_value & flag.0 != 0 {
                remaining_value &= !flag.0;
                if first {
                    first = false;
                } else {
                    write!(f, ", ")?;
                }
                write!(f, "{}", name)?;
            }
        }
        if remaining_value != 0 {
            if first {
                first = false;
            } else {
                write!(f, ", ")?;
            }
            write!(f, "{:#x?}", remaining_value)?;
        }
        write!(f, "])")?;
        Ok(())
    }
}

impl UserEvent {
    pub fn trigger(&self, data: usize) -> io::Result<()> {
        if !self.0.armed.swap(true, Ordering::SeqCst) {
            self.0.values.push(data);

            unsafe {
                let ptr = Arc::into_raw(self.0.clone());
                let mut event = libc::epoll_event {
                    events: libc::EPOLLIN as u32 | libc::EPOLLET as u32 | libc::EPOLLONESHOT as u32,
                    u64: ptr as usize as u64,
                };
                try_libc!(libc::epoll_ctl(self.0.queue.fd, libc::EPOLL_CTL_MOD, self.0.event_fd, &mut event), "failed to arm eventfd for UserEvent in epoll: {}");
            }

            loop {
                unsafe { 
                    let buffer = 1u64;
                    if libc::write(self.0.event_fd, &buffer as *const u64 as _, mem::size_of::<u64>()) < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(err);
                    }
                    break
                }
            }
        } else {
            self.0.values.push(data);
        }

        Ok(())
    }
}

#[derive(Default)]
struct FilterCollection<T> {
    read: T,
    write: T,
}

impl<T> FilterCollection<T> {
    fn try_new<E>(mut f: impl FnMut(Filter) -> Result<T, E>) -> Result<FilterCollection<T>, E> {
        Ok(FilterCollection {
            read: f(Filter::READ)?,
            write: f(Filter::WRITE)?,
        })
    }

    #[inline]
    fn get(&self, index: Filter) -> Option<&T> {
        match index {
            Filter::READ => Some(&self.read),
            Filter::WRITE => Some(&self.write),
            _ => None,
        }
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item = (Filter, &'a T)> + 'a {
        FilterCollectionIter {
            collection: self,
            next_filter: Some(Filter::READ),
        }
    }
}

struct FilterCollectionIter<'a, T> {
    collection: &'a FilterCollection<T>,
    next_filter: Option<Filter>,
}

impl<'a, T> Iterator for FilterCollectionIter<'a, T> {
    type Item = (Filter, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(filter) = self.next_filter.take() {
            let (next_filter, value) = match filter {
                Filter::READ => (Some(Filter::WRITE), &self.collection.read),
                Filter::WRITE => (None, &self.collection.write),
                _ => unreachable!(),
            };
            self.next_filter = next_filter;
            Some((filter, value))
        } else {
            None
        }
    }
}

impl<T> ops::Index<Filter> for FilterCollection<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Filter) -> &T {
        match index {
            Filter::READ => &self.read,
            Filter::WRITE => &self.write, 
            _ => panic!("invalid filter {:?} for indexing", index),
        }
    }
}

impl<T> ops::IndexMut<Filter> for FilterCollection<T> {
    #[inline]
    fn index_mut(&mut self, index: Filter) -> &mut T {
        match index {
            Filter::READ => &mut self.read,
            Filter::WRITE => &mut self.write,
            _ => panic!("invalid filter {:?} for indexing", index),
        }
    }
}