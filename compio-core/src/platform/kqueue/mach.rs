use super::{_EventQueue};

use std::{io, ptr};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Waker, LocalWaker, Poll};
use std::os::raw::{c_int};

use crossbeam::queue::SegQueue;
use libc::{self, c_void};

pub struct PortRegistration {
    inner: Arc<_PortRegistration>,
    // Used to keep spurious poll calls from filling up the Waker queue in the
    // RegistrationState.
    waker: Option<Waker>,
}

pub(super) struct _PortRegistration {
    // TODO: this pointer will be upgraded whenever a filter registration is made, which may cause a lot of fighting over the reference count
    // for this pointer. Maybe just warn when EventQueue is dropped and the Arc is not unique?
    queue: Weak<_EventQueue>,
    port: RawPort,
    // TODO: consider padding to avoid false sharing?
    pub(super) ready: AtomicBool,
    // TODO: Consider whether we want to go with an approach more like how Future.shared() is implemented (i.e. a Slab inside
    // a Mutex). It has the advantage that if the Registration moves from one task to another, the old Waker is disposed. This isn't a
    // huge issue since spurious wakeups are fine and migration of Futures is rare (especially now that Futures are pinned) but it's something
    // to consider.
    pub(super) listeners: SegQueue<Waker>,
}

#[cfg(target_os = "macos")]
pub type RawPort = std::os::raw::c_uint;

pub trait RegistrarExt {
    fn register_mach_port(&self, source: RawPort) -> io::Result<PortRegistration>;
}

impl RegistrarExt for crate::queue::Registrar {
    fn register_mach_port(&self, source: RawPort) -> io::Result<PortRegistration> {
        {
            let mut registered_mach_ports = self.inner.0.registered_mach_ports.lock().unwrap();
            if !registered_mach_ports.insert(source) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "the given mach port is already registered with this EventQueue"));
            }
        }

        Ok(PortRegistration {
            inner: Arc::new(_PortRegistration {
                queue: Arc::downgrade(&self.inner.0),
                port: source,
                ready: AtomicBool::new(true),
                listeners: Default::default(),
            }),
            waker: Default::default(),
        })
    }
}


impl PortRegistration {
    pub fn poll_recv_ready(&mut self, waker: &LocalWaker) -> Poll<io::Result<()>> {
        if self.inner.ready.load(Ordering::SeqCst) {
            self.waker = None;
            return Poll::Ready(Ok(()));
        }

        if let Some(existing_waker) = &self.waker {
            if waker.will_wake_nonlocal(existing_waker) {
                return Poll::Pending;
            }
        }

        self.inner.listeners.push(waker.clone().into());
        // Check state again, just in case there was a race
        if self.inner.ready.load(Ordering::SeqCst) {
            self.waker = None;
            return Poll::Ready(Ok(()));
        }
        self.waker = Some(waker.clone().into());

        Poll::Pending
    }

    pub fn clear_recv_ready(&mut self, waker: &LocalWaker) -> io::Result<()> {
        // If we're the first to set the state to false, it's our job to add this port to the
        // kqueue
        if self.inner.ready.swap(false, Ordering::SeqCst) {
            // Ensure we put our waker in the queue after we're sure we're on a not listening -> listening edge
            self.inner.listeners.push(waker.clone().into());
            unsafe {
                let ptr = Arc::into_raw(self.inner.clone());

                let queue = if let Some(queue) = self.inner.queue.upgrade() { queue } else {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "a request was made to register a file descriptor to receive events on an EventQueue, but that EventQueue has been dropped"));
                };

                let mut changes = [libc::kevent {
                    ident: self.inner.port as usize,
                    filter: libc::EVFILT_MACHPORT,
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
            self.inner.listeners.push(waker.clone().into());
        }
        self.waker = Some(waker.clone().into());

        Ok(())
    }
}

impl Clone for PortRegistration {
    fn clone(&self) -> PortRegistration {
        PortRegistration {
            inner: self.inner.clone(),
            waker: None,
        }
    }
}