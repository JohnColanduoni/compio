// SOCK_SEQPACKET-based Channel implementation. Unfortunately not supported on macOS.

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
            try_libc!(libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()), "socketpair() failed: {}");
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
            let byte_count = libc::recv(self.channel.inner.fd, self.buffer.as_mut_ptr() as _, self.buffer.len() as libc::size_t, 0);
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
            let byte_count = libc::send(self.channel.inner.fd, self.buffer.as_ptr() as _, self.buffer.len() as libc::size_t, 0);
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