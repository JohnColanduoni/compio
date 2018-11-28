#[cfg(unix)]
macro_rules! try_libc {
    (fd: $x:expr, $msg:tt $(,)* $($arg:expr),* $(,)*) => {
        {
            let fd = $x;
            if fd < 0 {
                let err = ::std::io::Error::last_os_error();
                error!($msg, err, $($arg)*);
                return ::std::result::Result::Err(err);
            }
            fd
        }
    };
    ($x:expr, $msg:tt $(,)* $($arg:expr),* $(,)*) => {
        if $x != 0 {
            let err = ::std::io::Error::last_os_error();
            error!($msg, err, $($arg)*);
            return ::std::result::Result::Err(err);
        }
    };
}

#[cfg_attr(not(target_os = "macos"), path = "seqpacket.rs")]
#[cfg_attr(target_os = "macos", path = "mach.rs")]
mod channel;

pub use self::channel::{Channel, PreChannel, ChannelExt};

use std::{io, mem};
use std::pin::{Pin, Unpin};
use std::sync::Arc;
use std::future::Future;
use std::task::{LocalWaker, Poll};
use std::os::unix::prelude::*;

use compio_core::queue::Registrar;
use compio_core::os::unix::*;
use futures_util::try_ready;

#[derive(Clone, Debug)]
pub struct Stream {
    inner: Arc<_Stream>,
    registration: Registration,
}

#[derive(Debug)]
struct _Stream {
    fd: RawFd,
}

impl Drop for _Stream {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

#[derive(Debug)]
pub struct PreStream {
    fd: RawFd,
}

impl Drop for PreStream {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

impl Stream {
    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> StreamReadFuture<'a> {
        StreamReadFuture {
            stream: self,
            buffer,
        }
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> StreamWriteFuture<'a> {
        StreamWriteFuture {
            stream: self,
            buffer,
        }
    }
}

pub trait StreamExt: Sized {
    fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self>;
}

impl StreamExt for crate::Stream {
    fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self> {
        let registration = queue.register_fd(fd)?;
        set_nonblock_and_cloexec(fd)?;

        let inner = Stream {
            inner: Arc::new(_Stream {
                fd,
            }),
            registration,
        };

        Ok(crate::Stream {
            inner,
        })
    }
}

impl AsRawFd for crate::Stream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.inner.fd
    }
}

impl PreStream {
    pub fn register(self, queue: &Registrar) -> io::Result<Stream> {
        let registration = queue.register_fd(self.fd)?;
        set_nonblock_and_cloexec(self.fd)?;
        let fd = self.fd;
        mem::forget(self);
        Ok(Stream {
            inner: Arc::new(_Stream {
                fd,
            }),
            registration,
        })
    }

    pub fn pair() -> io::Result<(PreStream, PreStream)> {
        unsafe {
            let mut fds: [RawFd; 2] = mem::zeroed();
            try_libc!(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()), "socketpair() failed: {}");
            // TODO: atomic CLOEXEC on Linux
            set_nonblock_and_cloexec(fds[0])?;
            set_nonblock_and_cloexec(fds[1])?;
            Ok((
                PreStream { fd: fds[0] },
                PreStream { fd: fds[1] },
            ))
        }
    }
}

impl FromRawFd for crate::PreStream {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        crate::PreStream {
            inner: PreStream {
                fd,
            },
        }
    }
}

pub struct StreamReadFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a mut [u8],
}

impl<'a> Unpin for StreamReadFuture<'a> {}

impl<'a> Future for StreamReadFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        try_ready!(self.stream.registration.poll_ready(Filter::READ, waker));

        let byte_count = loop {
            unsafe {
                let byte_count = libc::recv(self.stream.inner.fd, self.buffer.as_mut_ptr() as _, self.buffer.len() as libc::size_t, 0);
                if byte_count < 0 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::WouldBlock => {
                            self.stream.registration.clear_ready(Filter::READ, waker)?;
                            return Poll::Pending;
                        },
                        io::ErrorKind::Interrupted => continue,
                        _ => {
                            return Poll::Ready(Err(err));
                        },
                    }
                }
                break byte_count;
            }
        };
        
        Poll::Ready(Ok(byte_count as usize))
    }
}

pub struct StreamWriteFuture<'a> {
    stream: &'a mut Stream,
    buffer: &'a [u8],
}

impl<'a> Unpin for StreamWriteFuture<'a> {}

impl<'a> Future for StreamWriteFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        try_ready!(self.stream.registration.poll_ready(Filter::WRITE, waker));

        let byte_count = loop {
            unsafe {
                let byte_count = libc::send(self.stream.inner.fd, self.buffer.as_ptr() as _, self.buffer.len() as libc::size_t, 0);
                if byte_count < 0 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::WouldBlock =>  {
                            self.stream.registration.clear_ready(Filter::WRITE, waker)?;
                            return Poll::Pending;
                        },
                        io::ErrorKind::Interrupted => {},
                        _ => {
                            return Poll::Ready(Err(err));
                        },
                    }
                }
                break byte_count;
            }
        };
        Poll::Ready(Ok(byte_count as usize))
    }
}

fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let fd_flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if fd_flags == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_GETFD) failed: {}", err);
            return Err(err);
        }
        if libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_SETFD) failed: {}", err);
            return Err(err);
        }
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_GETFL) failed: {}", err);
            return Err(err);
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_SETFL) failed: {}", err);
            return Err(err);
        }

        Ok(())
    }
}
