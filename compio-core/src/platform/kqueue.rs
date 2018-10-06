use std::{io, mem, ptr, cmp, fmt, ops};
use std::collections::HashSet;
use std::sync::{
    Arc, Weak, Mutex,
    atomic::{Ordering, AtomicBool},
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
}

impl Drop for _EventQueue {
    fn drop(&mut self) {
        // FIXME: remove extant Arc<_Registration>s from queue and drop
        unsafe { libc::close(self.fd); }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Filter(i16);

pub struct Registration {
    inner: Arc<_Registration>,
    // Used to keep spurious poll calls from filling up the Waker queue in the
    // RegistrationState.
    wakers: FilterCollection<Option<Waker>>,
}

struct _Registration {
    // TODO: this pointer will be upgraded whenever a filter registration is made, which may cause a lot of fighting over the reference count
    // for this pointer. Maybe just warn when EventQueue is dropped and the Arc is not unique?
    queue: Weak<_EventQueue>,
    fd: RawFd,
    // TODO: consider padding to avoid false sharing?
    states: FilterCollection<RegistrationState>,
}

struct RegistrationState {
    ready: AtomicBool,
    // TODO: Consider whether we want to go with an approach more like how Future.shared() is implemented (i.e. a Slab inside
    // a Mutex). It has the advantage that if the Registration moves from one task to another, the old Waker is disposed. This isn't a
    // huge issue since spurious wakeups are fine and migration of Futures is rare (especially now that Futures are pinned) but it's something
    // to consider.
    listeners: SegQueue<Waker>,
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

impl EventQueue {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { try_libc!(fd: libc::kqueue(), "kqueue() failed: {}") };
        // TODO: use kqueue1 where available to avoid the usual CLOEXEC race
        unsafe { try_libc!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), "fcntl() failed: {}") };
        Ok(EventQueue(Arc::new(_EventQueue {
            fd,
            registered_fds: Default::default(),
        })))
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

                if event.filter & (libc::EVFILT_READ | libc::EVFILT_WRITE) != 0 {
                    // ident is a file descriptor, and udata is an Arc<_Registration>
                    let registration = Arc::from_raw(event.udata as *const _Registration);
                    let state = &registration.states[Filter(event.filter)];
                    // TODO: weaken?
                    if !state.ready.swap(true, Ordering::SeqCst) {
                        while let Some(waker) = state.listeners.try_pop() {
                            waker.wake();
                        }
                    }
                } else {
                    warn!("unknown filter {:#x?} received", event.filter);
                }
            }

            Ok(events.len())
        }
    }

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

impl Default for RegistrationState {
    fn default() -> Self {
        RegistrationState {
            ready: AtomicBool::new(true),
            listeners: Default::default(),
        }
    }
}

pub trait EventQueueExt {

}

impl Registration {
    pub fn poll_ready(&mut self, filter: Filter, waker: &LocalWaker) -> io::Result<Poll<()>> {
        let state = &self.inner.states[filter];
        // TODO: weaken?
        if state.ready.load(Ordering::SeqCst) {
            self.wakers[filter] = None;
            return Ok(Poll::Ready(()));
        }

        if let Some(existing_waker) = &self.wakers[filter] {
            if waker.will_wake_nonlocal(existing_waker) {
                return Ok(Poll::Pending);
            }
        }

        state.listeners.push(waker.clone().into());
        // Check state again, just in case there was a race
        // TODO: weaken?
        if state.ready.load(Ordering::SeqCst) {
            self.wakers[filter] = None;
            return Ok(Poll::Ready(()));
        }
        self.wakers[filter] = Some(waker.clone().into());

        Ok(Poll::Pending)
    }

    pub fn clear_ready(&mut self, filter: Filter, waker: &LocalWaker) -> io::Result<()> {
        let state = &self.inner.states[filter];
        // If we're the first to set the state to false, it's our job to add this (fd, filter) pair to the
        // kqueue
        if state.ready.swap(false, Ordering::SeqCst) {
            // Ensure we put our waker in the queue after we're sure we're on a not listening -> listening edge
            state.listeners.push(waker.clone().into());
            unsafe {
                let ptr = Arc::into_raw(self.inner.clone());

                let queue = if let Some(queue) = self.inner.queue.upgrade() { queue } else {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "a request was made to register a file descriptor to receive events on an EventQueue, but that EventQueue has been dropped"));
                };

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
        self.wakers[filter] = Some(waker.clone().into());

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