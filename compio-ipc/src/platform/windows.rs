use std::{io, mem, ptr, fmt};
use std::pin::{Pin};
use std::sync::Arc;
use std::future::Future;
use std::task::{LocalWaker, Poll};
use std::os::windows::prelude::*;

use compio_core::queue::Registrar;
use compio_core::os::windows::*;
use uuid::Uuid;
use winhandle::{WinHandle, winapi_handle_call, winapi_bool_call};
use widestring::U16CString;
use winapi::{
    shared::minwindef::{DWORD, TRUE},
    shared::winerror::{ERROR_IO_PENDING},
    um::minwinbase::{OVERLAPPED},
    um::winbase::{PIPE_ACCESS_DUPLEX, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, 
        PIPE_REJECT_REMOTE_CLIENTS, SECURITY_IDENTIFICATION, PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_READMODE_BYTE, PIPE_READMODE_MESSAGE},
    um::winnt::{GENERIC_READ, GENERIC_WRITE},
    um::errhandlingapi::{GetLastError},
    um::fileapi::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING},
    um::namedpipeapi::{CreateNamedPipeW, ConnectNamedPipe},
    um::ioapiset::{GetOverlappedResultEx},
};

#[derive(Clone)]
pub struct Stream {
    inner: Arc<_Stream>,
    operation_source: OperationSource,
}

struct _Stream {
    pipe: WinHandle,
}

#[derive(Debug)]
pub struct PreStream {
    pipe: WinHandle,
}


#[derive(Clone)]
pub struct Channel {
    inner: Arc<_Channel>,
    operation_source: OperationSource,
}

struct _Channel {
    pipe: WinHandle,
}

#[derive(Debug)]
pub struct PreChannel {
    pipe: WinHandle,
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
    unsafe fn from_raw_handle(handle: RawHandle, queue: &Registrar) -> io::Result<Self>;
}

impl StreamExt for crate::Stream {
    unsafe fn from_raw_handle(handle: RawHandle, queue: &Registrar) -> io::Result<Self> {
        let operation_source = queue.register_handle_raw(handle)?;

        let inner = Stream {
            inner: Arc::new(_Stream {
                pipe: WinHandle::from_raw_unchecked(handle as _),
            }),
            operation_source,
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

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &*self.inner)
    }
}

impl fmt::Debug for _Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Stream")
            .field("pipe", &self.pipe)
            .finish()
    }
}

impl PreStream {
    pub fn register(self, queue: &Registrar) -> io::Result<Stream> {
        let operation_source = queue.register_handle(&self.pipe)?;
        Ok(Stream {
            inner: Arc::new(_Stream {
                pipe: self.pipe,
            }),
            operation_source,
        })
    }

    pub fn pair() -> io::Result<(PreStream, PreStream)> {
        let (a, b) = raw_pipe_pair(PIPE_TYPE_BYTE | PIPE_READMODE_BYTE)?;
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

impl Channel {
    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> ChannelReadFuture<'a> {
        ChannelReadFuture {
            channel: self,
            buffer,
            operation: None,
        }
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> ChannelWriteFuture<'a> {
        ChannelWriteFuture {
            channel: self,
            buffer,
            operation: None,
        }
    }
}

pub trait ChannelExt: Sized {
    unsafe fn from_raw_handle(handle: RawHandle, queue: &Registrar) -> io::Result<Self>;
}

impl ChannelExt for crate::Channel {
    unsafe fn from_raw_handle(handle: RawHandle, queue: &Registrar) -> io::Result<Self> {
        let operation_source = queue.register_handle_raw(handle)?;

        let inner = Channel {
            inner: Arc::new(_Channel {
                pipe: WinHandle::from_raw_unchecked(handle as _),
            }),
            operation_source,
        };

        Ok(crate::Channel {
            inner,
        })
    }
}

impl AsRawHandle for crate::Channel {
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.inner.pipe.get() as _
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &*self.inner)
    }
}

impl fmt::Debug for _Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Channel")
            .field("pipe", &self.pipe)
            .finish()
    }
}

impl PreChannel {
    pub fn register(self, queue: &Registrar) -> io::Result<Channel> {
        let operation_source = queue.register_handle(&self.pipe)?;
        Ok(Channel {
            inner: Arc::new(_Channel {
                pipe: self.pipe,
            }),
            operation_source,
        })
    }

    pub fn pair() -> io::Result<(PreChannel, PreChannel)> {
        let (a, b) = raw_pipe_pair(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE)?;
        Ok((
            PreChannel { pipe: a },
            PreChannel { pipe: b },
        ))
    }
}

impl FromRawHandle for crate::PreChannel {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        crate::PreChannel {
            inner: PreChannel {
                pipe: WinHandle::from_raw_unchecked(handle as _),
            },
        }
    }
}

pub struct StreamReadFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a mut [u8],
    operation: Option<Operation>,
}

impl<'a> Future for StreamReadFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.stream.inner.pipe;
            let buffer = &mut this.buffer;
            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation),  &mut this.stream.operation_source, waker, move |overlapped| {
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

pub struct StreamWriteFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a [u8],
    operation: Option<Operation>,
}

impl<'a> Future for StreamWriteFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.stream.inner.pipe;
            let buffer = this.buffer;
            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut this.stream.operation_source, waker, move |overlapped| {
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

pub struct ChannelReadFuture<'a> {
    channel: &'a mut Channel,
    buffer: &'a mut [u8],
    operation: Option<Operation>,
}

impl<'a> Future for ChannelReadFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.channel.inner.pipe;
            let buffer = &mut this.buffer;
            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation),  &mut this.channel.operation_source, waker, move |overlapped| {
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

pub struct ChannelWriteFuture<'a> {
    channel: &'a mut Channel,
    buffer: &'a [u8],
    operation: Option<Operation>,
}

impl<'a> Future for ChannelWriteFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);
            let pipe = &this.channel.inner.pipe;
            let buffer = this.buffer;
            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut this.channel.operation_source, waker, move |overlapped| {
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
