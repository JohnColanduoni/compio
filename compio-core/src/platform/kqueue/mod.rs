#[cfg(target_os = "macos")]
pub mod mach;

use crate::queue::UserEventHandler;

use std::{io, mem, ptr, cmp, fmt, ops};
use std::collections::HashSet;
use std::sync::{
    Arc, Weak, Mutex,
    atomic::{Ordering, AtomicUsize},
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
    registered_fds: Mutex<HashSet<RawFd>>,
    #[cfg(target_os = "macos")]
    registered_mach_ports: Mutex<HashSet<self::mach::RawPort>>,
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
pub struct Filter(i16);

pub struct Registration {
    inner: Arc<_Registration>,
    // Used to keep spurious poll calls from filling up the Waker queue in the
    // RegistrationState. The `usize` is a sequence number from the RegistrationState, and
    // the Waker is a clone of the currently subscribed waker for the `will_wake` check.
    wakers: FilterCollection<Option<(usize, Waker)>>,
}

struct _Registration {
    // TODO: this pointer will be upgraded whenever a filter registration is made, which may cause a lot of fighting over the reference count
    // for this pointer. Maybe just warn when EventQueue is dropped and the Arc is not unique?
    queue: Weak<_EventQueue>,
    fd: RawFd,
    // TODO: consider padding to avoid false sharing?
    states: FilterCollection<RegistrationState>,
}

impl Drop for _Registration {
    fn drop(&mut self) {
        let queue = if let Some(queue) = self.queue.upgrade() { queue } else {
            return;
        };
        let mut registered_fds = queue.registered_fds.lock().unwrap();
        registered_fds.remove(&self.fd);
    }
}

struct RegistrationState {
    // When this value is even, the given state is "ready", an not ready when odd. The sequence number is used so 
    // it can be determined whether the current task is already subscribed to notifications.
    sequence: AtomicUsize,
    listeners: SegQueue<Waker>,
}

pub struct UserEvent(Arc<_UserEvent>);

// User events are implemented via EVFILT_USER, where the pointer to the _UserEvent struct is the identifier and the
// data parameter is passed in kevent.data (NOTE: *not* kevent.udata).
struct _UserEvent {
    queue: Arc<_EventQueue>,
    handler: UserEventHandler,
}

impl Drop for _UserEvent {
    fn drop(&mut self) {
        // TODO: unregister event from queue
    }
}


impl EventQueue {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { try_libc!(fd: libc::kqueue(), "kqueue() failed: {}") };
        // TODO: use kqueue1 where available to avoid the usual CLOEXEC race
        unsafe { try_libc!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), "fcntl() failed: {}") };
        Ok(EventQueue(Arc::new(_EventQueue {
            fd,
            registered_fds: Default::default(),
            #[cfg(target_os = "macos")]
            registered_mach_ports: Default::default(),
        })))
    }

    pub fn registrar(&self) -> Registrar {
        Registrar(self.0.clone())
    }

    pub fn turn(&self, max_wait: Option<Duration>, max_events: Option<usize>) -> io::Result<usize> {
        unsafe {
            let mut events: [libc::kevent; 64] = mem::zeroed();
            let timespec = max_wait.map(|x| libc::timespec {
                tv_sec: x.as_secs() as libc::time_t,
                tv_nsec: x.subsec_nanos() as c_long,
            });

            let event_count = libc::kevent(
                self.0.fd,
                ptr::null(),
                0,
                events.as_mut_ptr(),
                max_events.map(|x| cmp::min(x, events.len())).unwrap_or(events.len()) as c_int,
                timespec.as_ref().map(|x| x as *const _).unwrap_or(ptr::null()),
            );

            if event_count < 0 {
                let err = io::Error::last_os_error();
                error!("kevent failed: {}", err);
                return Err(err);
            }

            let events = &events[0..(event_count as usize)];

            for event in events.iter() {
                if event.flags & libc::EV_ERROR != 0 {
                    // TODO: think about what to do here
                    warn!("received EV_ERROR event");
                    continue;
                }

                match event.filter {
                    libc::EVFILT_READ | libc::EVFILT_WRITE => {
                        // ident is a file descriptor, and udata is an Arc<_Registration>
                        let registration = Arc::from_raw(event.udata as *const _Registration);
                        let state = &registration.states[Filter(event.filter)];
                        let old_seq = state.sequence.fetch_add(1, Ordering::SeqCst);
                        assert!(old_seq % 2 == 1, "registration should only be present in EVFILT_READ or EVFILT_WRITE event if it was most recently added to kqueue");
                        while let Some(waker) = state.listeners.try_pop() {
                            waker.wake();
                        }
                    },
                    #[cfg(target_os = "macos")]
                    libc::EVFILT_MACHPORT => {
                        // ident is a port, and udata is an Arc<_PortRegistration>
                        let registration = Arc::from_raw(event.udata as *const self::mach::_PortRegistration);
                        if !registration.ready.swap(true, Ordering::SeqCst) {
                            while let Some(waker) = registration.listeners.try_pop() {
                                waker.wake();
                            }
                        }
                    }
                    libc::EVFILT_USER => {
                        println!("received user event {:?} (flags: {:?})", event.data, event.flags);
                        // ident is a Arc<_UserEvent>, and data is a user-specified data parameter
                        let user_event = Arc::from_raw(event.ident as *const _UserEvent);
                        (user_event.handler)(event.data as usize);
                    },
                    filter => {
                        warn!("unknown filter {:#x?} received", filter);
                    }
                }
            }

            Ok(events.len())
        }
    }


    pub fn add_user_event(&self, handler: UserEventHandler) -> io::Result<UserEvent> {
        Ok(UserEvent(Arc::new(_UserEvent {
            queue: self.0.clone(),
            handler,
        })))
    }
}

impl Registrar {
    pub fn register_fd(&self, source: RawFd) -> io::Result<Registration> {
        {
            let mut registered_fds = self.0.registered_fds.lock().unwrap();
            if !registered_fds.insert(source) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "the given fd is already registered with this EventQueue"));
            }
        }

        Ok(Registration {
            inner: Arc::new(_Registration {
                queue: Arc::downgrade(&self.0),
                fd: source,
                states: Default::default(),
            }),
            wakers: Default::default(),
        })
    }
}

impl fmt::Debug for EventQueue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventQueue")
            .field("kqueue", &format_args!("{:?}", self.0.fd))
            .finish()
    }
}

impl fmt::Debug for Registrar {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Registrar")
            .field("kqueue", &format_args!("{:?}", self.0.fd))
            .finish()
    }
}

impl Default for RegistrationState {
    fn default() -> Self {
        RegistrationState {
            sequence: AtomicUsize::new(0),
            listeners: Default::default(),
        }
    }
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
        // If we're the first to set the state to not ready, it's our job to add this (fd, filter) pair to the kqueue
        let old_sequence = state.sequence.fetch_or(0x1, Ordering::SeqCst);
        if old_sequence % 2 == 0 {
            // Ensure we put our waker in the queue after we're sure we're on a not listening -> listening edge
            state.listeners.push(waker.clone().into());
            unsafe {
                let queue = if let Some(queue) = self.inner.queue.upgrade() { queue } else {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "a request was made to register a file descriptor to receive events on an EventQueue, but that EventQueue has been dropped"));
                };

                let ptr = Arc::into_raw(self.inner.clone());

                let mut changes = [libc::kevent {
                    ident: self.inner.fd as usize,
                    filter: filter.0,
                    flags: libc::EV_CLEAR | libc::EV_ONESHOT | libc::EV_RECEIPT | libc::EV_ADD,
                    fflags: 0,
                    data: 0,
                    udata: ptr as *mut c_void,
                }];

                let event_count = libc::kevent(
                    queue.fd,
                    changes.as_ptr(),
                    changes.len() as c_int,
                    changes.as_mut_ptr(),
                    changes.len() as c_int,
                    ptr::null(),
                );
                if event_count < 0 {
                    let err = io::Error::last_os_error();
                    error!("kevent() failed: {}", err);
                    // TODO: maybe we should put state back to true, in case this is a transient error?
                    return Err(err);
                }
                debug_assert_eq!(changes.len(), event_count as usize);

                for change in changes.iter() {
                    // EV_RECEIPT should set EV_ERROR, but this is done just to not interfere with existing events
                    debug_assert_ne!(change.flags & libc::EV_ERROR, 0);
                    
                    if change.data == 0 {
                        continue
                    }

                    return Err(io::Error::from_raw_os_error(change.data as i32));
                }
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
    pub const READ: Filter = Filter(libc::EVFILT_READ);
    pub const WRITE: Filter = Filter(libc::EVFILT_WRITE);
}

impl fmt::Debug for Filter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            libc::EVFILT_READ => write!(f, "Filter(Read)"),
            libc::EVFILT_WRITE => write!(f, "Filter(Write)"),
            _ => write!(f, "Filter({:#x?})", self.0),
        }
    }
}

impl UserEvent {
    pub fn trigger(&self, data: usize) -> io::Result<()> {
        unsafe {
            // Add an extra reference to the Arc underlying this struct. Ownership of this extra reference count
            // will be take on by the event.
            let extra_ref = self.0.clone();
            let mut changes = [
                libc::kevent {
                    ident: &*extra_ref as *const _UserEvent as usize,
                    filter: libc::EVFILT_USER,
                    flags: libc::EV_RECEIPT | libc::EV_ADD | libc::EV_CLEAR | libc::EV_ONESHOT,
                    fflags: libc::NOTE_TRIGGER,
                    data: data as isize,
                    udata: 0 as _, 
                },
            ];

            let event_count = libc::kevent(
                self.0.queue.fd,
                changes.as_ptr(),
                changes.len() as c_int,
                changes.as_mut_ptr(),
                changes.len() as c_int,
                ptr::null(), 
            );

            if event_count < 0 {
                let err = io::Error::last_os_error();
                error!("kevent() failed: {}", err);
                return Err(err);
            }
            debug_assert_eq!(changes.len(), event_count as usize);

            for change in changes.iter() {
                // EV_RECEIPT should set EV_ERROR, but this is done just to not interfere with existing events
                debug_assert_ne!(change.flags & libc::EV_ERROR, 0);
                
                if change.data == 0 {
                    continue
                }

                return Err(io::Error::from_raw_os_error(change.data as i32));
            }

            // Once we're sure the event has been added, make sure we keep around the extra reference now in
            // the event queue.
            mem::forget(extra_ref);

            Ok(())
        }
    }
}

#[derive(Default)]
struct FilterCollection<T> {
    read: T,
    write: T,
}

impl<T> ops::Index<Filter> for FilterCollection<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Filter) -> &T {
        match index.0 {
            libc::EVFILT_READ => &self.read,
            libc::EVFILT_WRITE => &self.write, 
            _ => unreachable!(),
        }
    }
}

impl<T> ops::IndexMut<Filter> for FilterCollection<T> {
    #[inline]
    fn index_mut(&mut self, index: Filter) -> &mut T {
        match index.0 {
            libc::EVFILT_READ => &mut self.read,
            libc::EVFILT_WRITE => &mut self.write, 
            _ => unreachable!(),
        }
    }
}