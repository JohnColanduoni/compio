use super::set_nonblock_and_cloexec;

use std::{mem, io};
use std::marker::Unpin;
use std::pin::Pin;
use std::sync::Arc;
use std::future::Future;
use std::task::{Poll, Context};
use std::io::Result;
use std::net::SocketAddr;
use std::os::unix::prelude::*;

use compio_core::queue::Registrar;
use compio_core::os::unix::*;
use libc;
use futures_util::ready;
use net2::TcpBuilder;

#[derive(Clone, Debug)]
pub struct TcpListener {
    inner: Arc<_TcpListener>,
    registration: Registration,
}

#[derive(Debug)]
struct _TcpListener {
    std: std::net::TcpListener,
    queue: Registrar,
}

#[derive(Clone, Debug)]
pub struct TcpStream {
    inner: Arc<_TcpStream>,
    registration: Registration,
}

#[derive(Debug)]
struct _TcpStream {
    std: std::net::TcpStream,
}

impl TcpListener {
    pub fn bind(addr: &SocketAddr, queue: &Registrar) -> Result<TcpListener> {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::from_std(listener, queue)
    }

    pub fn from_std(listener: std::net::TcpListener, queue: &Registrar) -> Result<TcpListener> {
        set_nonblock_and_cloexec(listener.as_raw_fd())?;
        let registration = queue.register_fd(listener.as_raw_fd())?;
        Ok(TcpListener {
            inner: Arc::new(_TcpListener {
                std: listener,
                queue: queue.clone(),
            }),
            registration,
        })
    }

    pub fn as_std(&self) -> &std::net::TcpListener {
        &self.inner.std
    }

    pub fn accept<'a>(&'a mut self) -> AcceptFuture<'a> {
        AcceptFuture {
            listener: self,
        }
    }
}

impl TcpStream {
    pub fn connect<'a>(addr: SocketAddr, queue: &'a Registrar) -> ConnectFuture<'a> {
        ConnectFuture {
            addr,
            queue,
            socket: None,
        }
    }

    pub fn from_std(stream: std::net::TcpStream, queue: &Registrar) -> Result<TcpStream> {
        set_nonblock_and_cloexec(stream.as_raw_fd())?;
        let registration = queue.register_fd(stream.as_raw_fd())?;
        Ok(TcpStream {
            inner: Arc::new(_TcpStream {
                std: stream,
            }),
            registration,
        })
    }

    pub fn as_std(&self) -> &std::net::TcpStream {
        &self.inner.std
    }

    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> ReadFuture<'a> {
        ReadFuture {
            stream: self,
            buffer,
        }
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> WriteFuture<'a> {
        WriteFuture {
            stream: self,
            buffer,
        }
    }
}

pub struct AcceptFuture<'a> {
    listener: &'a mut TcpListener,
}

impl<'a> Unpin for AcceptFuture<'a> {}

impl<'a> Future for AcceptFuture<'a> {
    type Output = Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Result<TcpStream>> {
        ready!(self.listener.registration.poll_ready(Filter::READ, context.waker())?);

        unsafe {
            let mut remote_addr: libc::sockaddr = mem::zeroed();
            let mut addr_len: libc::socklen_t = mem::size_of_val(&remote_addr) as _;
            // TODO: atomic cloexec with accept4 on Linux
            let socket = libc::accept(self.listener.inner.std.as_raw_fd(), &mut remote_addr, &mut addr_len);
            if socket < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    self.listener.registration.clear_ready(Filter::READ, context.waker())?;
                    return Poll::Pending;
                } else {
                    debug!("accept failed: {}", err);
                    return Poll::Ready(Err(err));
                }
            }
            Poll::Ready(TcpStream::from_std(std::net::TcpStream::from_raw_fd(socket), &self.listener.inner.queue))
        }
    }
}

pub struct ConnectFuture<'a> {
    addr: SocketAddr,
    queue: &'a Registrar,
    socket: Option<TcpStream>,
}

impl<'a> Unpin for ConnectFuture<'a> {}

impl<'a> Future for ConnectFuture<'a> {
    type Output = Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Result<TcpStream>> {
        if self.socket.is_none() {
            let std_stream = if self.addr.is_ipv4() {
                TcpBuilder::new_v4()?.to_tcp_stream()?
            } else {
                TcpBuilder::new_v6()?.to_tcp_stream()?
            };
            // TODO: atomic cloexec with SOCK_CLOEXEC on Linux? Not sure if TcpBuilder does that already.
            set_nonblock_and_cloexec(std_stream.as_raw_fd())?;
            let registration = self.queue.register_fd(std_stream.as_raw_fd())?;
            let mut socket = TcpStream {
                inner: Arc::new(_TcpStream {
                    std: std_stream,
                }),
                registration,
            };
            unsafe {
                let (sockaddr, sockaddr_len) = socket_addr_to_ptrs(&self.addr);
                if libc::connect(socket.inner.std.as_raw_fd(), sockaddr, sockaddr_len) != 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINPROGRESS) {
                        socket.registration.clear_ready(Filter::WRITE, context.waker())?;
                        self.socket = Some(socket);
                        return Poll::Pending;
                    }
                    debug!("connect failed: {:?}", err);
                    return Poll::Ready(Err(err));
                } else {
                    return Poll::Ready(Ok(socket))
                }
            }
        }

        {
            let socket = self.socket.as_mut().unwrap();

            ready!(socket.registration.poll_ready(Filter::WRITE, context.waker())?);

            if let Err(err) = socket.inner.std.take_error() {
                return Poll::Ready(Err(err));
            }
        }

        Poll::Ready(Ok(self.socket.take().unwrap()))
    }
}

pub struct ReadFuture<'a> {
    stream: &'a mut TcpStream,
    buffer: &'a mut [u8],
}

unsafe impl<'a> Send for ReadFuture<'a> {}

impl<'a> Future for ReadFuture<'a> {
    type Output = Result<usize>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Result<usize>> {
        ready!(self.stream.registration.poll_ready(Filter::READ, context.waker())?);

        let byte_count = loop {
            unsafe {
                let byte_count = libc::recv(self.stream.inner.std.as_raw_fd(), self.buffer.as_mut_ptr() as _, self.buffer.len() as libc::size_t, 0);
                if byte_count < 0 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::WouldBlock => {
                            self.stream.registration.clear_ready(Filter::READ, context.waker())?;
                            return Poll::Pending;
                        },
                        io::ErrorKind::Interrupted => continue,
                        _ => {
                            debug!("recv failed: {}", err);
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

pub struct WriteFuture<'a> {
    stream: &'a mut TcpStream,
    buffer: &'a [u8],
}

unsafe impl<'a> Send for WriteFuture<'a> {}

impl<'a> Future for WriteFuture<'a> {
    type Output = Result<usize>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context) -> Poll<Result<usize>> {
        ready!(self.stream.registration.poll_ready(Filter::WRITE, context.waker())?);

        let byte_count = loop {
            unsafe {
                let byte_count = libc::send(self.stream.inner.std.as_raw_fd(), self.buffer.as_ptr() as _, self.buffer.len() as libc::size_t, 0);
                if byte_count < 0 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::WouldBlock => {
                            self.stream.registration.clear_ready(Filter::WRITE, context.waker())?;
                            return Poll::Pending;
                        },
                        io::ErrorKind::Interrupted => {},
                        _ => {
                            debug!("recv failed: {}", err);
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

fn socket_addr_to_ptrs(addr: &SocketAddr) -> (*const libc::sockaddr, libc::socklen_t) {
    match *addr {
        SocketAddr::V4(ref a) => {
            (a as *const _ as *const _, mem::size_of_val(a) as libc::socklen_t)
        }
        SocketAddr::V6(ref a) => {
            (a as *const _ as *const _, mem::size_of_val(a) as libc::socklen_t)
        }
    }
}