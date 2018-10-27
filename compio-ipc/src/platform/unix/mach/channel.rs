// Mach port based Channel implementation. Only supported on macOS.
use super::{Port, PortMsgBuffer};

use std::{io, fmt, mem, slice};
use std::pin::{Pin, Unpin};
use std::time::Duration;
use std::sync::Arc;
use std::future::Future;
use std::task::{LocalWaker, Poll};

use compio_core::queue::Registrar;
use compio_core::os::macos::*;
use futures_util::try_ready;

pub struct Channel {
    inner: Arc<_Channel>,
    rx_registration: PortRegistration,
    msg_buffer: Option<PortMsgBuffer>,
}

#[derive(Debug)]
struct _Channel {
    tx: Port,
    rx: Port,
}

#[derive(Debug)]
pub struct PreChannel {
    tx: Port,
    rx: Port,
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
    //fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self>;
}

impl ChannelExt for crate::Channel {
    /*fn from_raw_fd(fd: RawFd, queue: &Registrar) -> io::Result<Self> {
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
    }*/
}

impl Clone for Channel {
    fn clone(&self) -> Channel {
        Channel {
            inner: self.inner.clone(),
            rx_registration: self.rx_registration.clone(),
            msg_buffer: None,
        }
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Channel")
            .field("tx", &self.inner.tx)
            .field("rx", &self.inner.rx)
            .finish()
    }
}

impl PreChannel {
    pub fn register(self, queue: &Registrar) -> io::Result<Channel> {
        let rx_registration = queue.register_mach_port(self.rx.as_raw_port())?;
        Ok(Channel {
            inner: Arc::new(_Channel {
                tx: self.tx,
                rx: self.rx
            }),
            rx_registration,
            msg_buffer: None,
        })
    }

    pub fn pair() -> io::Result<(PreChannel, PreChannel)> {
        let left_to_right_rx = Port::new()?;
        let right_to_left_rx = Port::new()?;
        let left_to_right_tx = left_to_right_rx.make_sender()?;
        let right_to_left_tx = right_to_left_rx.make_sender()?;
        Ok((
            PreChannel {
                tx: left_to_right_tx,
                rx: right_to_left_rx,
            },
            PreChannel {
                tx: right_to_left_tx,
                rx: left_to_right_rx,
            },
        ))
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
        let this = &mut *self;
        try_ready!(this.channel.rx_registration.poll_recv_ready(waker));
        let msg_buffer = acquire_buffer(&mut this.channel.msg_buffer);
        msg_buffer.reserve_inline(mem::size_of::<usize>() + this.buffer.len());
        match this.channel.inner.rx.recv(msg_buffer, Some(Duration::new(0, 0))) {
            Ok(()) => (),
            Err(ref err) if err.kind() == io::ErrorKind::TimedOut => {
                this.channel.rx_registration.clear_recv_ready(waker)?;
                return Poll::Pending;
            },
            Err(err) => return Poll::Ready(Err(err)),
        }
        if msg_buffer.inline_data().len() < mem::size_of::<usize>() {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "inline data in mach message too short")));
        }
        let (len_bytes, data) = msg_buffer.inline_data().split_at(mem::size_of::<usize>());
        let length = unsafe { *(len_bytes.as_ptr() as *const usize) };
        let source = data.get(..length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "inline data in mach message does not match declared length"))?;
        let dest = this.buffer.get_mut(..length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "buffer too small for incomming message"))?;
        dest.copy_from_slice(source);
        Poll::Ready(Ok(length))
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
        // FIXME: implement async send with MACH_SEND_TIMEOUT and MACH_SEND_NOTIFY
        let this = &mut *self;
        let msg_buffer = acquire_buffer(&mut this.channel.msg_buffer);
        let size = this.buffer.len();
        msg_buffer.extend_inline_data(unsafe { slice::from_raw_parts(&size as *const usize as *const u8, mem::size_of::<usize>()) });
        msg_buffer.extend_inline_data(this.buffer);
        // Round data up to 4 byte boundary, due to mach message requirements
        match msg_buffer.inline_data().len() % mem::size_of::<u32>() {
            0 => {},
            c => {
                msg_buffer.extend_inline_data(&[0u8; mem::size_of::<u32>()][..(mem::size_of::<u32>() - c)]);
            },
        }
        this.channel.inner.tx.send(msg_buffer)?;
        Poll::Ready(Ok(()))
    }
}

#[inline]
fn acquire_buffer(msg_buffer: &mut Option<PortMsgBuffer>) -> &mut PortMsgBuffer {
    if let Some(buffer) = msg_buffer {
        buffer.reset();
        buffer
    } else {
        *msg_buffer = Some(PortMsgBuffer::new());
        msg_buffer.as_mut().unwrap()
    }
}