use crate::queue::UserEventHandler;

use std::{io, ptr, mem, cmp, fmt};
use std::marker::Pinned;
use std::cell::UnsafeCell;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Poll, LocalWaker, Waker, UnsafeWake};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use std::os::windows::prelude::*;

use futures_util::task::{AtomicWaker};
use winhandle::{WinHandle, WinHandleRef, winapi_handle_call, winapi_bool_call};
use winapi::{
    shared::minwindef::{DWORD, ULONG, UCHAR, TRUE, FALSE},
    shared::basetsd::{ULONG_PTR},
    shared::winerror::{WAIT_TIMEOUT, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NOT_FOUND},
    um::minwinbase::{OVERLAPPED_ENTRY, OVERLAPPED},
    um::winbase::{INFINITE, SetFileCompletionNotificationModes},
    um::winnt::{STATUS_PENDING},
    um::errhandlingapi::{GetLastError},
    um::handleapi::{INVALID_HANDLE_VALUE},
    um::ioapiset::{CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus,
        GetOverlappedResultEx, CancelIoEx},
};

pub struct EventQueue(Arc<_EventQueue>);

struct _EventQueue {
    iocp: WinHandle,
}

#[repr(C)]
struct _Token {
    kind: TokenKind,
}

enum TokenKind {
    OperationSource,
    UserEvent,
}

pub struct OperationSource {
    shared: Arc<_OperationSource>,
    avail_overlapped: Option<Arc<OperationOverlapped>>,
}

#[repr(C)] // Must start with _Token
struct _OperationSource {
    token: _Token,
    object: OperationSourceObject,
}

enum OperationSourceObject {
    Handle(RawHandle),
    Socket(RawSocket),
}

unsafe impl Send for OperationSourceObject {}
unsafe impl Sync for OperationSourceObject {}

// NB: At the moment, I haven't found a good way to include the OVERLAPPED within the (pinned) Operation instance
// in a way that can deal with operation cancelation safely. In my tests (Win 10) the OVERLAPPED structure did not
// reflect CancelIoEx until it made it through the IOCP. This means waiting in Drop (which is already somewhat problematic,
// as some devices, WinSock filters, etc. might not process cancelations in a timely manner) is not reliable for EventQueues
// with only a single poller. In the meantime, I've kept the Operation(Source) API compatible with eventually finding a solution
// to this problem. A couple thoughts about how to possible to overcome this:
//
// * Have single-threaded EventQueues declare themselves (boo hiss)
// * Detect when the EventQueue is not polled by another thread in a timely mannger, and call GetQueuedCompletionStatus inside 
//     Operation's Drop when canceling and requeue any unrelated events in that case. This is kind of complicated and may distort
//     performance/scheduling.
//
// The current situation is far from untenable, but it is slightly disadvantageous in situations where OperationSource will be cloned with high
// frequency (e.g. multiplexed IPC).
pub struct Operation {
    overlapped: Option<Arc<OperationOverlapped>>,
    source: Arc<_OperationSource>,
}

#[repr(C)] // We need OVERLAPPED to be at the very start of the structure
struct OperationOverlapped {
    overlapped: UnsafeCell<OVERLAPPED>,
    waker: AtomicWaker,
    _pinned: Pinned,
}

unsafe impl Send for OperationOverlapped {}
unsafe impl Sync for OperationOverlapped {}

impl Drop for Operation {
    fn drop(&mut self) {
        // Attempt to cancel operation if it is still pending
        if let Some(overlapped) = self.overlapped.take() {
            match self.source.object {
                OperationSourceObject::Handle(handle) => unsafe {
                    let overlapped_ptr = overlapped.overlapped.get();
                    if log_enabled!(log::Level::Trace) {
                        trace!("cancelling operation on handle {:?} backed by OperationOverlapped instance {:?}", WinHandleRef::from_raw_unchecked(handle as _), &*overlapped as *const _);
                    }
                    if CancelIoEx(handle as _, overlapped_ptr) == FALSE {
                        match GetLastError() {
                            ERROR_NOT_FOUND => {
                                // Operation is finished, just fall back to our normal deletion routine
                            },
                            code => {
                                let err = io::Error::from_raw_os_error(code as i32);
                                error!("CancelIoEx failed: {}. Leaking OVERLAPPED instance to prevent memory unsafety :(", err);
                                // Clear AtomicWaker so we don't cause a Waker instance to get leaked (this can result in huge memory leaks, because
                                // Wakers will often keep the memory associated with a spawned Future alive)
                                overlapped.waker.register(&noop_local_waker()); 
                                mem::forget(overlapped);
                                return
                            }
                        }
                    }
                },
                OperationSourceObject::Socket(_) => unimplemented!(),
            }
        }
    }
}

impl Drop for OperationOverlapped {
    fn drop(&mut self) {
        trace!("releasing OperationOverlapped instance at address {:?}", self as *const _);
    }
}

pub struct UserEvent(Arc<_UserEvent>);

// User events are implemented via PostQueuedCompletionStatus, where the pointer to the _UserEvent struct is the
// completion key and the value is passed as lpOverlapped.
#[repr(C)]
struct _UserEvent {
    token: _Token,
    queue: Arc<_EventQueue>,
    handler: UserEventHandler,
}

impl EventQueue {
    pub fn new() -> io::Result<EventQueue> {
        Self::with_max_concurrency(0)
    }

    pub fn with_max_concurrency(count: usize) -> io::Result<EventQueue> {
        let iocp = unsafe { winapi_handle_call!(log: CreateIoCompletionPort(
            INVALID_HANDLE_VALUE,
            ptr::null_mut(),
            0,
            count as DWORD,
        ) != ptr::null_mut())? };

        Ok(EventQueue(Arc::new(_EventQueue {
            iocp,
        })))
    }

    
    pub fn turn(&self, max_wait: Option<Duration>, max_events: Option<usize>) -> io::Result<usize> {
       unsafe {
           let mut events: [OVERLAPPED_ENTRY; 64] = mem::zeroed();
           let mut events_count: ULONG = 0;

           if GetQueuedCompletionStatusEx(
               self.0.iocp.get(),
               events.as_mut_ptr(),
               max_events.map(|x| cmp::min(x, events.len())).unwrap_or(events.len()) as DWORD,
               &mut events_count,
               max_wait.map(|x| (x.as_secs() * 1000) as DWORD + x.subsec_millis() as DWORD).unwrap_or(INFINITE),
               FALSE,
           ) != TRUE {
               match GetLastError() {
                   // Timeout is okay
                   WAIT_TIMEOUT if max_wait.is_some() => {
                       return Ok(0);
                   },
                   error => {
                       return Err(io::Error::from_raw_os_error(error as i32));
                   },
               }
           }
           let events = &events[..events_count as usize];

           for event in events.iter() {
               let token = event.lpCompletionKey as *const _Token;
               match (*token).kind {
                   TokenKind::OperationSource => {
                        let overlapped = Arc::from_raw(event.lpOverlapped as *const OperationOverlapped);
                        overlapped.waker.wake();
                   },
                   TokenKind::UserEvent => {
                       let user_event = Arc::from_raw(token as *const _UserEvent);
                       (user_event.handler)(event.lpOverlapped as usize);
                   },
               }
           }

           Ok(events.len())
       }
    }

    pub fn add_user_event(&self, handler: UserEventHandler) -> io::Result<UserEvent> {
       Ok(UserEvent(Arc::new(_UserEvent {
           token: _Token {
               kind: TokenKind::UserEvent,
           },
           queue: self.0.clone(),
           handler,
       })))
    }
}

pub trait EventQueueExt {
    fn register_handle(&self, handle: &impl AsRawHandle) -> io::Result<OperationSource>;
    unsafe fn register_handle_raw(&self, handle: RawHandle) -> io::Result<OperationSource>;
}

impl EventQueueExt for crate::queue::EventQueue {
    fn register_handle(&self, handle: &impl AsRawHandle) -> io::Result<OperationSource> {
        unsafe { self.register_handle_raw(handle.as_raw_handle()) }
    }

    unsafe fn register_handle_raw(&self, handle: RawHandle) -> io::Result<OperationSource> {
        // We use SetFileCompletionNotificationModes so the IOCP is not notified 
        // when an IO call returns immediately. Doing so is not only ineffecient, it may cause
        // memory unsafety if the overlapped structure is dropped early.
        winapi_bool_call!(log: SetFileCompletionNotificationModes(
            handle as _,
            FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
        ))?;

        let source = Arc::new(_OperationSource {
            token: _Token {
                kind: TokenKind::OperationSource,
            },
            object: OperationSourceObject::Handle(handle),
        });

        if CreateIoCompletionPort(
            handle as _,
            self.inner.0.iocp.get(),
            Arc::into_raw(source.clone()) as ULONG_PTR,
            0
        ) == ptr::null_mut() {
            let err = io::Error::last_os_error();
            error!("CreateIoCompletionPort failed: {}", err);
            return Err(err);
        }

        Ok(OperationSource {
            shared: source,
            avail_overlapped: None,
        })
    }
}

impl OperationSource {
    /// Begins an overlapped operation.
    /// 
    /// The operation will be treated as started if and only if the `f` returns `ERROR_IO_PENDING`. Success and other errors
    /// will assume the operation was not started or completed immediately. It is critical for memory safety that 
    /// users maintain this invariant, since submitted operations will contain pointers to the deinitialized structure.
    /// 
    /// The handle provided must be the source of the IO completion notifications, suitable for use with
    /// `GetOverlappedResultEx` and `CancelIoEx`.
    pub unsafe fn start_operation<F, R>(&mut self, dest: Pin<&mut Option<Operation>>, waker: &LocalWaker, f: F) -> Poll<io::Result<R>> where
        F: FnOnce(*mut OVERLAPPED) -> io::Result<R>,
    {
        assert!(dest.is_none(), "operation already in progress");

        let dest = Pin::get_mut_unchecked(dest);
        let overlapped = self.avail_overlapped.take()
            // Ensure the IOCP no longer has a handle to this OperationOverlapped before we try to reuse it (simply discard it if that is not the case)
            .filter(|x| Arc::strong_count(x) == 1)
            .unwrap_or_else(|| {
                let instance = Arc::new(OperationOverlapped {
                    overlapped: mem::zeroed(),
                    waker: AtomicWaker::new(),
                    _pinned: Pinned,
                });
                trace!("fabricating new OperationOverlapped at address {:?}", &*instance as *const _);
                instance
            });
        *dest = Some(Operation {
            overlapped: Some(overlapped),
            source: self.shared.clone(),
        });
        let operation = dest.as_mut().unwrap();
        let overlapped = operation.overlapped.as_ref().unwrap();

        overlapped.waker.register(waker);
        let extra_ref = overlapped.clone(); // Create an extra reference for the IOCP
        match f(overlapped.overlapped.get()) {
            Ok(x) => {
                *dest = None;
                Poll::Ready(Ok(x))
            },
            Err(ref err) if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) => {
                mem::forget(extra_ref); // Leave the extra reference for the IOCP to cleanup
                Poll::Pending
            },
            Err(err) => {
                *dest = None;
                Poll::Ready(Err(err))
            },
        }
    }
}

impl Clone for OperationSource {
    fn clone(&self) -> OperationSource {
        OperationSource {
            shared: self.shared.clone(),
            avail_overlapped: None,
        }
    }
}

impl fmt::Debug for OperationSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &*self.shared)
    }
}

impl fmt::Debug for _OperationSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("OperationSource")
            .field("object", &self.object)
            .finish()
    }
}

impl fmt::Debug for OperationSourceObject {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            OperationSourceObject::Handle(handle) => {
                fmt::Debug::fmt(unsafe { WinHandleRef::from_raw_unchecked(handle as _) }, f)
            },
            OperationSourceObject::Socket(_socket) => {
                write!(f, "Socket")
            }
        }
    }
}

impl Operation {
    /// Polls the status of the `Operation`, optionally
    /// 
    /// This function is unsafe because the caller must ensure the OperationSource is the same one used to create the operation.
    pub unsafe fn poll(self: Pin<&mut Operation>, source: &mut OperationSource, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        let overlapped = self.overlapped.as_ref().expect("operation already polled to completion");
        let overlapped_ptr = overlapped.overlapped.get();

        // Make sure we set waker before the check, so we don't miss a wakeup
        overlapped.waker.register(waker);
        let internal_atomic = &*(&(*overlapped_ptr).Internal as *const usize as *const AtomicUsize);
        let result = match internal_atomic.load(Ordering::SeqCst) as u32 {
            STATUS_PENDING => {
                Poll::Pending
            },
            _ => {
                match self.source.object {
                    OperationSourceObject::Handle(handle) => {
                        let mut bytes_transferred: DWORD = 0;
                        if GetOverlappedResultEx(
                            handle as _,
                            overlapped_ptr,
                            &mut bytes_transferred,
                            0,
                            FALSE,
                        ) == FALSE {
                            match GetLastError() {
                                ERROR_IO_INCOMPLETE => return Poll::Pending,
                                code => {
                                    let err = io::Error::from_raw_os_error(code as i32);
                                    return Poll::Ready(Err(err));
                                },
                            }
                        }
                        
                        Poll::Ready(Ok(bytes_transferred as usize))
                    },
                    OperationSourceObject::Socket(_socket) => {
                        unimplemented!()
                    },
                }
            }
        };

        if !result.is_pending() {
            // Give the overlapped back to our OperationSource for reuse
            source.avail_overlapped = Pin::get_mut_unchecked(self).overlapped.take();
        }

        result
    }

    /// Executes the `Operation` pattern used by most overlapped IO system calls.
    /// 
    /// If there is no operation in progress, this function calls `start`. If `start` returns `Ok`, the operation is assumed to
    /// have been completed instantly and no `Operation` is created. If it returns `ERROR_IO_PENDING`, an operation is started. Other errors
    /// pass through without starting an operation.
    /// 
    /// If there is an operation in progress, it simply polls the operation for completion.
    #[inline]
    pub unsafe fn start_or_poll<F>(this: Pin<&mut Option<Operation>>, source: &mut OperationSource, waker: &LocalWaker, start: F) -> Poll<io::Result<usize>> where
        F: FnOnce(*mut OVERLAPPED) -> io::Result<usize>
    {
        if this.is_none() {
            source.start_operation(this, waker, start)
        } else {
            Self::poll(Pin::map_unchecked_mut(this,|x| x.as_mut().unwrap()), source, waker)
        }
    }
}

impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Operation")
            .field("source", &self.source)
            .finish()
    }
}

impl UserEvent {
    pub fn trigger(&self, data: usize) -> io::Result<()> {
        unsafe {
            // Add an extra reference to the Arc underlying this struct. Ownership of this extra reference count
            // will be take on by the event.
            let extra_ref = self.0.clone();
            winapi_bool_call!(PostQueuedCompletionStatus(
                self.0.queue.iocp.get(),
                0,
                &*extra_ref as *const _UserEvent as ULONG_PTR,
                data as _,
            ))?;

            // Once we're sure the event has been added, make sure we keep around the extra reference now in
            // the event queue.
            mem::forget(extra_ref);

            Ok(())
        }
    }
}

const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: UCHAR = 0x1;
const FILE_SKIP_SET_EVENT_ON_HANDLE: UCHAR = 0x2;

// A waker used to clear an AtomicTask
struct NoopWake;

unsafe impl UnsafeWake for NoopWake {
    unsafe fn clone_raw(&self) -> Waker {
        noop_waker()
    }

    unsafe fn drop_raw(&self) {}

    unsafe fn wake(&self) {}
}

fn noop_unsafe_wake() -> NonNull<dyn UnsafeWake> {
    static mut INSTANCE: NoopWake = NoopWake;
    unsafe { NonNull::new_unchecked(&mut INSTANCE as *mut dyn UnsafeWake) }
}

fn noop_waker() -> Waker {
    unsafe { Waker::new(noop_unsafe_wake()) }
}

fn noop_local_waker() -> LocalWaker {
    unsafe { LocalWaker::new(noop_unsafe_wake()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Operation>();
    }

    #[test]
    fn operation_source_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<OperationSource>();
    }
}