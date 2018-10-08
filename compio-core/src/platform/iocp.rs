use crate::queue::UserEventHandler;

use std::{io, ptr, mem, cmp};
use std::marker::Pinned;
use std::cell::UnsafeCell;
use std::pin::Pin;
use std::task::{Poll, LocalWaker};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use std::os::windows::prelude::*;

use futures_util::task::AtomicWaker;
use winhandle::{WinHandle, winapi_handle_call, winapi_bool_call};
use winapi::{
    shared::minwindef::{DWORD, ULONG, UCHAR, TRUE, FALSE},
    shared::basetsd::{ULONG_PTR},
    shared::winerror::{WAIT_TIMEOUT, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING},
    um::minwinbase::{OVERLAPPED_ENTRY, OVERLAPPED},
    um::winbase::{INFINITE, SetFileCompletionNotificationModes},
    um::winnt::{STATUS_PENDING},
    um::errhandlingapi::{GetLastError},
    um::handleapi::{INVALID_HANDLE_VALUE},
    um::ioapiset::{CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus, GetOverlappedResultEx},
};

pub struct EventQueue(Arc<_EventQueue>);

struct _EventQueue {
    iocp: WinHandle,
}

pub struct Operation {
    overlapped: OperationOverlapped,
    source: RawHandle,
}

unsafe impl Send for Operation {}

#[repr(C)] // We need OVERLAPPED to be at the very start of the structure
struct OperationOverlapped {
    overlapped: UnsafeCell<OVERLAPPED>,
    waker: AtomicWaker,
    _pinned: Pinned,
}

pub struct UserEvent(Arc<_UserEvent>);

// User events are implemented via PostQueuedCompletionStatus, where the pointer to the _UserEvent struct is the
// completion key and the value is passed as lpOverlapped.
struct _UserEvent {
    queue: Arc<_EventQueue>,
    handler: UserEventHandler,
}

const HANDLE_COMPLETION_KEY: ULONG_PTR = 1;
const SOCKET_COMPLETION_KEY: ULONG_PTR = 2;

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
               match event.lpCompletionKey {
                   HANDLE_COMPLETION_KEY | SOCKET_COMPLETION_KEY => {
                       let overlapped = &*(event.lpOverlapped as *const OperationOverlapped);
                       overlapped.waker.wake();
                   },
                   // The only other keys should be pointers to _UserEvents
                   key => {
                       let user_event = Arc::from_raw(key as *const _UserEvent);
                       (user_event.handler)(event.lpOverlapped as usize);
                   },
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

pub trait EventQueueExt {
    fn register_handle(&self, handle: &impl AsRawHandle) -> io::Result<()>;
    unsafe fn register_handle_raw(&self, handle: RawHandle) -> io::Result<()>;
}

impl EventQueueExt for crate::queue::EventQueue {
    fn register_handle(&self, handle: &impl AsRawHandle) -> io::Result<()> {
        unsafe { self.register_handle_raw(handle.as_raw_handle()) }
    }

    unsafe fn register_handle_raw(&self, handle: RawHandle) -> io::Result<()> {
        // We use SetFileCompletionNotificationModes so the IOCP is not notified 
        // when an IO call returns immediately. Doing so is not only ineffecient, it may cause
        // memory unsafety if the overlapped structure is dropped early.
        winapi_bool_call!(log: SetFileCompletionNotificationModes(
            handle as _,
            FILE_SKIP_COMPLETION_PORT_ON_SUCCESS,
        ))?;

        if CreateIoCompletionPort(
            handle as _,
            self.inner.0.iocp.get(),
            HANDLE_COMPLETION_KEY,
            0
        ) == ptr::null_mut() {
            let err = io::Error::last_os_error();
            error!("CreateIoCompletionPort failed: {}", err);
            return Err(err);
        }

        Ok(())
    }
}

impl Operation {
    /// Begins an overlapped operation.
    /// 
    /// The operation will be treated as started if and only if the `f` returns `ERROR_IO_PENDING`. Success and other errors
    /// will assume the operation was not started. It is critical for memory safety that users maintain this invariant, since
    /// submitted operations will contain pointers to the deinitialized structure.
    /// 
    /// The handle provided must be the source of the IO completion notifications, suitable for use with
    /// `CancelIoEx`.
    pub unsafe fn start<F, R>(dest: Pin<&mut Option<Operation>>, waker: &LocalWaker, source: RawHandle, f: F) -> Poll<io::Result<R>> where
        F: FnOnce(*mut OVERLAPPED) -> io::Result<R>
    {
        assert!(dest.is_none(), "operation already in progress");

        let dest = Pin::get_mut_unchecked(dest);
        *dest = Some(Operation {
            overlapped: OperationOverlapped {
                overlapped: mem::zeroed(),
                waker: AtomicWaker::new(),
                _pinned: Pinned,
            },
            source,
        });
        let operation = dest.as_mut().unwrap();

        operation.overlapped.waker.register(waker);
        match f(operation.overlapped.overlapped.get()) {
            Ok(x) => {
                *dest = None;
                Poll::Ready(Ok(x))
            },
            Err(ref err) if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) => {
                Poll::Pending
            },
            Err(err) => {
                *dest = None;
                Poll::Ready(Err(err))
            },
        }
    }

    pub unsafe fn poll_handle_raw(self: Pin<&mut Operation>, waker: &LocalWaker, handle: RawHandle) -> Poll<io::Result<usize>> {
        let overlapped = self.overlapped.overlapped.get();

        // Make sure we set waker before the check, so we don't miss a wakeup
        self.overlapped.waker.register(waker);
        let internal_atomic = &*(&(*overlapped).Internal as *const usize as *const AtomicUsize);
        match internal_atomic.load(Ordering::SeqCst) as u32 { // TODO: weaken?
            STATUS_PENDING => {
                Poll::Pending
            },
            _ => {
                let mut bytes_transferred: DWORD = 0;
                if GetOverlappedResultEx(
                    handle as _,
                    overlapped,
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
        }
    }

     pub unsafe fn poll_handle(self: Pin<&mut Operation>, waker: &LocalWaker, handle: &impl AsRawHandle) -> Poll<io::Result<usize>> {
        Self::poll_handle_raw(self, waker, handle.as_raw_handle())
    }

    /// Executes the `Operation` pattern used by most overlapped IO system calls.
    /// 
    /// If there is no operation in progress, this function calls `start`. If `start` returns `Ok`, the operation is assumed to
    /// have been completed instantly and no `Operation` is created. If it returns `ERROR_IO_PENDING`, an operation is started. Other errors
    /// pass through without starting an operation.
    /// 
    /// If there is an operation in progress, it simply polls the operation for completion.
    #[inline]
    pub unsafe fn start_or_poll_handle_raw<F>(this: Pin<&mut Option<Operation>>, waker: &LocalWaker, handle: RawHandle, start: F) -> Poll<io::Result<usize>> where
        F: FnOnce(*mut OVERLAPPED) -> io::Result<usize>
    {
        if this.is_none() {
            Self::start(this, waker, handle, start)
        } else {
            Self::poll_handle_raw(Pin::map_unchecked_mut(this, |x| x.as_mut().unwrap()), waker, handle)
        }
    }

    /// Executes the `Operation` pattern used by most overlapped IO system calls.
    /// 
    /// If there is no operation in progress, this function calls `start`. If `start` returns `Ok`, the operation is assumed to
    /// have been completed instantly and no `Operation` is created. If it returns `ERROR_IO_PENDING`, an operation is started. Other errors
    /// pass through without starting an operation.
    /// 
    /// If there is an operation in progress, it simply polls the operation for completion.
    #[inline]
    pub unsafe fn start_or_poll_handle<F>(this: Pin<&mut Option<Operation>>, waker: &LocalWaker, handle: &impl AsRawHandle, start: F) -> Poll<io::Result<usize>> where
        F: FnOnce(*mut OVERLAPPED) -> io::Result<usize>
    {
        Self::start_or_poll_handle_raw(this, waker, handle.as_raw_handle(), start)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Operation>();
    }
}