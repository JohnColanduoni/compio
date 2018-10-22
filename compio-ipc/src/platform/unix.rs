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

#[derive(Clone, Debug)]
pub struct Channel {
    inner: Arc<_Channel>,
    registration: Registration,
}

#[derive(Debug)]
struct _Channel {
    fd: RawFd,
}

impl Drop for _Channel {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

#[derive(Debug)]
pub struct PreChannel {
    fd: RawFd,
}

impl Drop for PreChannel {
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


impl Channel {
    pub fn recv<'a>(&'a mut self, buffer: &'a mut [u8]) -> ChannelRecvFuture<'a> {
        ChannelRecvFuture {
            channel: self,
            buffer,
        }
    }

    pub fn send<'a>(&'a mut self, buffer: &'a [u8]) -> ChannelSendFuture<'a> {
        ChannelSendFuture {
            channel: self,
            buffer,
        }
    }
}

pub trait ChannelExt: Sized {
    fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self>;
}

impl ChannelExt for crate::Channel {
    fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self> {
        let registration = queue.register_fd(fd)?;
        set_nonblock_and_cloexec(fd)?;

        let inner = Channel {
            inner: Arc::new(_Channel {
                fd,
            }),
            registration,
        };

        Ok(crate::Channel {
            inner,
        })
    }
}

impl AsRawFd for crate::Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.inner.fd
    }
}

impl PreChannel {
    pub fn register(self, queue: &Registrar) -> io::Result<Channel> {
        let registration = queue.register_fd(self.fd)?;
        set_nonblock_and_cloexec(self.fd)?;
        let fd = self.fd;
        mem::forget(self);
        Ok(Channel {
            inner: Arc::new(_Channel {
                fd,
            }),
            registration,
        })
    }

    pub fn pair() -> io::Result<(PreChannel, PreChannel)> {
        unsafe {
            let mut fds: [RawFd; 2] = mem::zeroed();
            // macOS doesn't support SOCK_SEQPACKET. It does however allow SCM_RIGHTS messages to be sent over a SOCK_STREAM socket, and in this case it
            // preserves message boundaries. This behavior is not documented but it used heavily in Chromium.
            let socket_type = if cfg!(target_os = "macos") {
                libc::SOCK_STREAM
            } else {
                libc::SOCK_SEQPACKET
            };
            try_libc!(libc::socketpair(libc::AF_UNIX, socket_type, 0, fds.as_mut_ptr()), "socketpair() failed: {}");
            // TODO: atomic CLOEXEC on Linux
            set_nonblock_and_cloexec(fds[0])?;
            set_nonblock_and_cloexec(fds[1])?;
            Ok((
                PreChannel { fd: fds[0] },
                PreChannel { fd: fds[1] },
            ))
        }
    }
}

impl FromRawFd for crate::PreChannel {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        crate::PreChannel {
            inner: PreChannel {
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

        unsafe {
            let byte_count = libc::recv(self.stream.inner.fd, self.buffer.as_mut_ptr() as _, self.buffer.len() as libc::size_t, 0);
            if byte_count < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    self.stream.registration.clear_ready(Filter::READ, waker)?;
                    return Poll::Pending;
                } else {
                    return Poll::Ready(Err(err));
                }
            }
            Poll::Ready(Ok(byte_count as usize))
        }
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

        unsafe {
            let byte_count = libc::send(self.stream.inner.fd, self.buffer.as_ptr() as _, self.buffer.len() as libc::size_t, 0);
            if byte_count < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    self.stream.registration.clear_ready(Filter::WRITE, waker)?;
                    return Poll::Pending;
                } else {
                    return Poll::Ready(Err(err));
                }
            }
            Poll::Ready(Ok(byte_count as usize))
        }
    }
}


pub struct ChannelRecvFuture<'a> {
    channel: &'a mut Channel,
    buffer: &'a mut [u8],
}

impl<'a> Unpin for ChannelRecvFuture<'a> {}

impl<'a> Future for ChannelRecvFuture<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<usize>> {
        try_ready!(self.channel.registration.poll_ready(Filter::READ, waker));

        unsafe {
            #[cfg(not(target_os = "macos"))]
            let byte_count = libc::recv(self.channel.inner.fd, self.buffer.as_mut_ptr() as _, self.buffer.len() as libc::size_t, 0);
            #[cfg(target_os = "macos")]
            let byte_count = {
                let mut msg: libc::msghdr = mem::zeroed();
                let mut cmsg_buf = [0u8; CMSG_LEN(0)];
                let mut iovec: libc::iovec = mem::zeroed();
                msg.msg_control = cmsg_buf.as_mut_ptr() as _;
                msg.msg_controllen = cmsg_buf.len() as _;
                msg.msg_iov = &mut iovec;
                msg.msg_iovlen = 1;
                iovec.iov_base = self.buffer.as_ptr() as _;
                iovec.iov_len = self.buffer.len() as _;
                let cmsg = &mut *(cmsg_buf.as_mut_ptr() as *mut libc::cmsghdr);
                cmsg.cmsg_len = cmsg_buf.len() as _;
                cmsg.cmsg_level = libc::SOL_SOCKET;
                cmsg.cmsg_type = libc::SCM_RIGHTS;
                libc::recvmsg(self.channel.inner.fd, &mut msg, 0)
            };
            if byte_count < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    self.channel.registration.clear_ready(Filter::READ, waker)?;
                    return Poll::Pending;
                } else {
                    debug!("recv failed: {}", err);
                    return Poll::Ready(Err(err));
                }
            }
            Poll::Ready(Ok(byte_count as usize))
        }
    }
}

pub struct ChannelSendFuture<'a> {
    channel: &'a mut Channel,
    buffer: &'a [u8],
}

impl<'a> Unpin for ChannelSendFuture<'a> {}

impl<'a> Future for ChannelSendFuture<'a> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<()>> {
        try_ready!(self.channel.registration.poll_ready(Filter::WRITE, waker));

        unsafe {
            #[cfg(not(target_os = "macos"))]
            let byte_count = libc::send(self.channel.inner.fd, self.buffer.as_ptr() as _, self.buffer.len() as libc::size_t, 0);
            // To preserve message boundaries on macOS, we need to always send a message with SCM_RIGHTS (which may be empty)
            #[cfg(target_os = "macos")]
            let byte_count = {
                let mut msg: libc::msghdr = mem::zeroed();
                let mut cmsg_buf = [0u8; CMSG_LEN(0)];
                let mut iovec: libc::iovec = mem::zeroed();
                msg.msg_control = cmsg_buf.as_ptr() as *const libc::cmsghdr as _;
                msg.msg_controllen = cmsg_buf.len() as _;
                msg.msg_iov = &mut iovec;
                msg.msg_iovlen = 1;
                let cmsg = &mut *(cmsg_buf.as_mut_ptr() as *mut libc::cmsghdr);
                cmsg.cmsg_level = libc::SOL_SOCKET;
                cmsg.cmsg_type = libc::SCM_RIGHTS;
                cmsg.cmsg_len = cmsg_buf.len() as _;
                iovec.iov_base = self.buffer.as_ptr() as _;
                iovec.iov_len = self.buffer.len() as _;
                libc::sendmsg(self.channel.inner.fd, &msg, 0)
            };
            if byte_count != self.buffer.len() as isize {
                let err;
                if byte_count < 0 {
                    err = io::Error::last_os_error();
                } else {
                    err = io::Error::new(io::ErrorKind::Other, "entire message was not sent");
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    self.channel.registration.clear_ready(Filter::WRITE, waker)?;
                    return Poll::Pending;
                } else {
                    debug!("send failed: {}", err);
                    return Poll::Ready(Err(err));
                }
            }
            Poll::Ready(Ok(()))
        }
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

#[allow(non_snake_case)]
const fn CMSG_LEN(length: libc::size_t) -> libc::size_t {
    CMSG_ALIGN(mem::size_of::<libc::cmsghdr>()) + length
}

#[allow(non_snake_case)]
unsafe fn CMSG_DATA(cmsg: *mut libc::cmsghdr) -> *mut libc::c_void {
    (cmsg as *mut libc::c_uchar).offset(CMSG_ALIGN(
            mem::size_of::<libc::cmsghdr>()) as isize) as *mut libc::c_void
}

#[allow(non_snake_case)]
const fn CMSG_ALIGN(length: libc::size_t) -> libc::size_t {
    (length + mem::size_of::<libc::size_t>() - 1) & !(mem::size_of::<libc::size_t>() - 1)
}

#[allow(non_snake_case)]
const fn CMSG_SPACE(length: libc::size_t) -> libc::size_t {
    CMSG_ALIGN(length) + CMSG_ALIGN(mem::size_of::<libc::cmsghdr>())
}