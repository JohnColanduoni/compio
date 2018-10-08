use std::{io, mem, ptr};
use std::pin::{Pin, Unpin};
use std::sync::Arc;
use std::future::Future;
use std::task::{LocalWaker, Poll};
use std::ffi::{OsString};
use std::os::windows::prelude::*;

use compio_core::queue::EventQueue;
use compio_core::os::windows::*;
use uuid::Uuid;
use futures_util::try_ready;
use winhandle::{WinHandle, winapi_handle_call, winapi_bool_call};
use widestring::U16CString;
use winapi::{
    shared::minwindef::{DWORD, TRUE},
    shared::winerror::{ERROR_IO_PENDING},
    um::minwinbase::{OVERLAPPED},
    um::winbase::{PIPE_ACCESS_DUPLEX, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, 
        PIPE_REJECT_REMOTE_CLIENTS, SECURITY_IDENTIFICATION, PIPE_TYPE_BYTE},
    um::winnt::{GENERIC_READ, GENERIC_WRITE},
    um::errhandlingapi::{GetLastError},
    um::fileapi::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING},
    um::namedpipeapi::{CreateNamedPipeW, ConnectNamedPipe},
    um::ioapiset::{GetOverlappedResultEx},
};

#[derive(Clone, Debug)]
pub struct Stream {
    inner: Arc<_Stream>,
}

#[derive(Debug)]
struct _Stream {
    pipe: WinHandle,
}

#[derive(Debug)]
pub struct PreStream {
    pipe: WinHandle,
}

pub struct StreamReadFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a mut [u8],
    operation: Option<Operation>,
}

pub struct StreamWriteFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a [u8],
    operation: Option<Operation>,
}

impl Stream {
    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> StreamReadFuture<'a> {
        StreamReadFuture {
            stream: self,
            buffer,
            operation: None,
        }
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> StreamWriteFuture<'a> {
        StreamWriteFuture {
            stream: self,
            buffer,
            operation: None,
        }
    }
}

pub trait StreamExt: Sized {
    unsafe fn from_raw_handle(handle: RawHandle, event_queue: &EventQueue) -> io::Result<Self>;
}

impl StreamExt for crate::Stream {
    unsafe fn from_raw_handle(handle: RawHandle, event_queue: &EventQueue) -> io::Result<Self> {
        event_queue.register_handle_raw(handle)?;

        let inner = Stream {
            inner: Arc::new(_Stream {
                pipe: WinHandle::from_raw_unchecked(handle as _),
            }),
        };

        Ok(crate::Stream {
            inner,
        })
    }
}

impl AsRawHandle for crate::Stream {
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.inner.pipe.get() as _
    }
}

impl<'a> Unpin for StreamReadFuture<'a> {}

impl<'a> Future for StreamReadFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.stream.inner.pipe;
            let buffer = &mut this.buffer;
            Operation::start_or_poll_handle(Pin::new_unchecked(&mut this.operation), waker, &this.stream.inner.pipe, move |overlapped| {
                let mut bytes_read: DWORD = 0;
                winapi_bool_call!(ReadFile(
                    pipe.get(),
                    buffer.as_mut_ptr() as _,
                    buffer.len() as DWORD,
                    &mut bytes_read,
                    overlapped,
                ))?;
                Ok(bytes_read as usize)
            })
        }
    }
}

impl<'a> Unpin for StreamWriteFuture<'a> {}

impl<'a> Future for StreamWriteFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.stream.inner.pipe;
            let buffer = this.buffer;
            Operation::start_or_poll_handle(Pin::new_unchecked(&mut this.operation), waker, &this.stream.inner.pipe, move |overlapped| {
                let mut bytes_written: DWORD = 0;
                winapi_bool_call!(WriteFile(
                    pipe.get(),
                    buffer.as_ptr() as _,
                    buffer.len() as DWORD,
                    &mut bytes_written,
                    overlapped,
                ))?;
                Ok(bytes_written as usize)
            })
        }
    }
}


impl PreStream {
    pub fn register(self, event_queue: &EventQueue) -> io::Result<Stream> {
        event_queue.register_handle(&self.pipe)?;
        Ok(Stream {
            inner: Arc::new(_Stream {
                pipe: self.pipe,
            }),
        })
    }

    pub fn pair() -> io::Result<(PreStream, PreStream)> {
        let (a, b) = raw_pipe_pair(PIPE_TYPE_BYTE)?;
        Ok((
            PreStream { pipe: a },
            PreStream { pipe: b },
        ))
    }
}

impl FromRawHandle for crate::PreStream {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        crate::PreStream {
            inner: PreStream {
                pipe: WinHandle::from_raw_unchecked(handle as _),
            },
        }
    }
}

// Anonymous pipes unfortunately don't support overlapped IO, so we need to create a pair of connected named pipes.
// This is safe because the pipes are connection-based and cannot be interfered with once connected.
fn raw_pipe_pair(pipe_type: DWORD) -> io::Result<(WinHandle, WinHandle)> {
    unsafe {
        let pipe_name = format!(r#"\\.\pipe\LOCAL\{}"#, Uuid::new_v4());

        debug!("creating named pipe pair {:?}", pipe_name);
        let server_pipe = winapi_handle_call!(log: CreateNamedPipeW(
            U16CString::from_str(&pipe_name).unwrap().as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            pipe_type | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0, 0,
            0,
            ptr::null_mut(),
        ))?;

        // Begin connection operation on server half
        let mut overlapped: OVERLAPPED = mem::zeroed();
        if ConnectNamedPipe(
            server_pipe.get(),
            &mut overlapped
        ) != 0 {
            error!("ConnectNamedPipe returned unexpectedly");
            return Err(io::Error::new(io::ErrorKind::Other, "ConnectNamedPipe returned unexpectedly"));
        }

        match GetLastError() {
            ERROR_IO_PENDING => {},
            // In theory we could get an ERROR_PIPE_CONNECTED, but in that case it's not us on the other
            // end so we don't want the connection anyway.
            code => {
                let err = io::Error::from_raw_os_error(code as i32);
                error!("ConnectNamedPipe failed: {}", err);
                return Err(err);
            },
        }

        // Connect to the server end
        let client_pipe = winapi_handle_call!(log: CreateFileW(
            U16CString::from_str(&pipe_name).unwrap().as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null_mut(),
            OPEN_EXISTING,
            SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        ))?;

        // Finish server side of the connect operation
        let mut bytes: DWORD = 0;
        winapi_bool_call!(log: GetOverlappedResultEx(
            server_pipe.get(),
            &mut overlapped,
            &mut bytes,
            1000,
            TRUE
        ))?;

        Ok((server_pipe, client_pipe))
    }
}
