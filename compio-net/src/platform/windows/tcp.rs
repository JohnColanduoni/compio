use std::{ptr, mem, io};
use std::pin::Pin;
use std::sync::Arc;
use std::future::Future;
use std::task::{Poll, LocalWaker};
use std::io::{Result};
use std::net::{SocketAddr, IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::windows::prelude::*;

use compio_core::queue::{Registrar};
use compio_core::os::windows::*;
use net2::TcpBuilder;
use winapi::{
    ctypes::{c_int},
    shared::minwindef::{DWORD, LPDWORD, BOOL, FALSE, ULONG},
    shared::ntdef::{PVOID},
    shared::guiddef::GUID,
    shared::ws2def::{SIO_GET_EXTENSION_FUNCTION_POINTER},
    um::minwinbase::{LPOVERLAPPED},
    shared::ws2def::{SOCKADDR, SOCKADDR_IN, WSABUF},
    shared::ws2ipdef::{SOCKADDR_IN6_LH},
    um::winsock2::{WSAGetLastError, WSAIoctl, WSASend, WSARecv, setsockopt, SOCKET, SOL_SOCKET},
};

#[derive(Clone, Debug)]
pub struct TcpListener {
    inner: Arc<_TcpListener>,
    operation_source: OperationSource,
}

#[derive(Debug)]
struct _TcpListener {
    std: std::net::TcpListener,
    queue: Registrar,
}

#[derive(Clone, Debug)]
pub struct TcpStream {
    inner: Arc<_TcpStream>,
    operation_source: OperationSource,
}

#[derive(Debug)]
struct _TcpStream {
    std: std::net::TcpStream,
}

macro_rules! winsock_bool_call {
    (log_overlapped: $f:ident($($arg:expr),*$(,)*)) => {
        if $f($($arg,)*) == FALSE {
            match ::winapi::um::winsock2::WSAGetLastError() {
                code @ ::winapi::um::winsock2::WSA_IO_PENDING => {
                    let err = ::std::io::Error::from_raw_os_error(code);
                    Err(err)
                },
                code => {
                    let err = ::std::io::Error::from_raw_os_error(code);
                    error!("{} failed: {}", stringify!($f), err);
                    Err(err)
                },
            }
        } else {
            Ok(())
        }
    };
}

macro_rules! winsock_zero_call {
    (log: $f:ident($($arg:expr),*$(,)*)) => {
        if $f($($arg,)*) != 0 {
            let err = ::std::io::Error::from_raw_os_error(::winapi::um::winsock2::WSAGetLastError());
            error!("{} failed: {}", stringify!($f), err);
            Err(err)
        } else {
            Ok(())
        }
    };
    (log_overlapped: $f:ident($($arg:expr),*$(,)*)) => {
        if $f($($arg,)*) != 0 {
            match ::winapi::um::winsock2::WSAGetLastError() {
                code @ ::winapi::um::winsock2::WSA_IO_PENDING => {
                    let err = ::std::io::Error::from_raw_os_error(code);
                    Err(err)
                },
                code => {
                    let err = ::std::io::Error::from_raw_os_error(code);
                    error!("{} failed: {}", stringify!($f), err);
                    Err(err)
                },
            }
        } else {
            Ok(())
        }
    };
}

impl TcpListener {
    pub fn bind(addr: &SocketAddr, queue: &Registrar) -> Result<TcpListener> {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::from_std(listener, queue)
    }

    pub fn from_std(listener: std::net::TcpListener, queue: &Registrar) -> Result<TcpListener> {
        let operation_source = queue.register_socket(&listener)?;
        Ok(TcpListener {
            inner: Arc::new(_TcpListener {
                std: listener,
                queue: queue.clone(),
            }),
            operation_source,
        })
    }

    pub fn as_std(&self) -> &std::net::TcpListener {
        &self.inner.std
    }

    pub fn accept<'a>(&'a mut self) -> AcceptFuture<'a> {
        AcceptFuture {
            listener: self,
            accept_socket: None,
            operation: None,
            address_buffer: [0u8; 256],
        }
    }
}

impl TcpStream {
    pub fn connect<'a>(addr: SocketAddr, queue: &'a Registrar) -> ConnectFuture<'a> {
        ConnectFuture {
            addr,
            queue,
            socket: None,
            operation: None,
        }
    }

    pub fn from_std(stream: std::net::TcpStream, queue: &Registrar) -> Result<TcpStream> {
        let operation_source = queue.register_socket(&stream)?;
        Ok(TcpStream {
            inner: Arc::new(_TcpStream {
                std: stream,
            }),
            operation_source,
        })
    }

    pub fn as_std(&self) -> &std::net::TcpStream {
        &self.inner.std
    }

    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> ReadFuture<'a> {
        ReadFuture {
            stream: self,
            operation: None,
            wsabuf: WSABUF {
                len: buffer.len() as ULONG,
                buf: buffer.as_mut_ptr() as _,
            },
            _buffer: buffer,
        }
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> WriteFuture<'a> {
        WriteFuture {
            stream: self,
            operation: None,
            wsabuf: WSABUF {
                len: buffer.len() as ULONG,
                buf: buffer.as_ptr() as _,
            },
            _buffer: buffer,
        }
    }
}

pub struct AcceptFuture<'a> {
    listener: &'a mut TcpListener,
    accept_socket: Option<TcpStream>,
    operation: Option<Operation>,
    address_buffer: [u8; 256], // TODO: this is overkill
}

impl<'a> Future for AcceptFuture<'a> {
    type Output = Result<TcpStream>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<Result<TcpStream>> {
        let this = unsafe { Pin::get_mut_unchecked(self) };
        if this.accept_socket.is_none() {
            // Windows requires the socket be created prior to connection
            let local_addr = this.listener.inner.std.local_addr()?;
            let std_stream = if local_addr.is_ipv4() {
                TcpBuilder::new_v4()?.to_tcp_stream()?
            } else {
                TcpBuilder::new_v6()?.to_tcp_stream()?
            };
            let operation_source = this.listener.inner.queue.register_socket(&std_stream)?;
            this.accept_socket = Some(TcpStream {
                inner: Arc::new(_TcpStream {
                    std: std_stream,
                }),
                operation_source,
            });
        }

        unsafe {
            let accept_socket = this.accept_socket.as_mut().unwrap();

            let accept_ex = mem::transmute::<_, LPFN_ACCEPTEX>(get_winsock_function(this.listener.inner.std.as_raw_socket() as _, &WSAID_ACCEPTEX).unwrap()).unwrap();
            let std_listen = &this.listener.inner.std;
            let std_accept = &accept_socket.inner.std;
            let address_buffer = &mut this.address_buffer;

            match Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut this.listener.operation_source, waker, |overlapped| {
                let mut bytes_received: DWORD = 0;
                winsock_bool_call!(log_overlapped: accept_ex(
                    std_listen.as_raw_socket() as _,
                    std_accept.as_raw_socket() as _,
                    address_buffer.as_mut_ptr() as _,
                    0,
                    128,
                    128,
                    &mut bytes_received,
                    overlapped,
                ))?;
                Ok(bytes_received as usize)
            })? {
                Poll::Ready(_) => {
                    // AcceptEx does not set accept context
                    winsock_zero_call!(log: setsockopt(
                         std_accept.as_raw_socket() as _,
                         SOL_SOCKET,
                         SO_UPDATE_ACCEPT_CONTEXT,
                         &std_listen.as_raw_socket() as *const RawSocket as _,
                         mem::size_of::<RawSocket>() as _, 
                    ))?;
                },
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(this.accept_socket.take().unwrap()))
    }
}

pub struct ConnectFuture<'a> {
    addr: SocketAddr,
    queue: &'a Registrar,
    socket: Option<TcpStream>,
    operation: Option<Operation>,
}

impl<'a> Future for ConnectFuture<'a> {
    type Output = Result<TcpStream>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<Result<TcpStream>> {
        let this = unsafe { Pin::get_mut_unchecked(self) };
        if this.socket.is_none() {
            // Windows requires the socket be bound prior to connection
            let std_stream = if this.addr.is_ipv4() {
                TcpBuilder::new_v4()?.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0))?.to_tcp_stream()?
            } else {
                TcpBuilder::new_v6()?.bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)), 0))?.to_tcp_stream()?
            };
            let operation_source = this.queue.register_socket(&std_stream)?;
            this.socket = Some(TcpStream {
                inner: Arc::new(_TcpStream {
                    std: std_stream,
                }),
                operation_source,
            });
        }

        unsafe {
            let socket = this.socket.as_mut().unwrap();

            let connect_ex = mem::transmute::<_, LPFN_CONNECTEX>(get_winsock_function(socket.inner.std.as_raw_socket() as _, &WSAID_CONNECTEX).unwrap()).unwrap();

            let std_stream = &socket.inner.std;
            let (addr_ptr, addr_len) = socket_addr_to_ptrs(&this.addr);
            match Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut socket.operation_source, waker, |overlapped| {
                winsock_bool_call!(log_overlapped: connect_ex(
                    std_stream.as_raw_socket() as _,
                    addr_ptr,
                    addr_len,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    overlapped,
                ))?;
                Ok(0)
            })? {
                Poll::Ready(_) => {
                    // ConnectEx does not set connection context
                    winsock_zero_call!(log: setsockopt(
                         std_stream.as_raw_socket() as _,
                         SOL_SOCKET,
                         SO_UPDATE_CONNECT_CONTEXT,
                         ptr::null_mut(),
                         0, 
                    ))?;
                },
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(this.socket.take().unwrap()))
    }
}

pub struct ReadFuture<'a> {
    stream: &'a mut TcpStream,
    _buffer: &'a mut [u8],
    operation: Option<Operation>,
    // WSABUF must be pinned, not just buffer
    wsabuf: WSABUF,
}

unsafe impl<'a> Send for ReadFuture<'a> {}

impl<'a> Future for ReadFuture<'a> {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);

            let std_socket = &this.stream.inner.std;
            let wsabuf = &mut this.wsabuf;

            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut this.stream.operation_source, waker, move |overlapped| {
                let mut bytes_transferred: DWORD = 0;
                let mut flags: DWORD = 0;
                winsock_zero_call!(log_overlapped: WSARecv(
                    std_socket.as_raw_socket() as _,
                    wsabuf,
                    1,
                    &mut bytes_transferred,
                    &mut flags,
                    overlapped,
                    None,
                ))?;
                Ok(bytes_transferred as usize)
            })
        }
    }
}

pub struct WriteFuture<'a> {
    stream: &'a mut TcpStream,
    _buffer: &'a [u8],
    operation: Option<Operation>,
    // WSABUF must be pinned, not just buffer
    wsabuf: WSABUF,
}

unsafe impl<'a> Send for WriteFuture<'a> {}

impl<'a> Future for WriteFuture<'a> {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<Result<usize>> {
        unsafe {
            let this = Pin::get_mut_unchecked(self);

            let std_socket = &this.stream.inner.std;
            let wsabuf = &mut this.wsabuf;

            Operation::start_or_poll(Pin::new_unchecked(&mut this.operation), &mut this.stream.operation_source, waker, move |overlapped| {
                let mut bytes_transferred: DWORD = 0;
                winsock_zero_call!(log_overlapped: WSASend(
                    std_socket.as_raw_socket() as _,
                    wsabuf,
                    1,
                    &mut bytes_transferred,
                    0,
                    overlapped,
                    None,
                ))?;
                Ok(bytes_transferred as usize)
            })
        }
    }
}

const WSAID_CONNECTEX: GUID = GUID {
    Data1: 0x25a207b9,
    Data2: 0xddf3,
    Data3: 0x4660,
    Data4: [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e],
};
const WSAID_ACCEPTEX: GUID = GUID {
    Data1: 0xb5367df1,
    Data2: 0xcbac,
    Data3: 0x11cf,
    Data4: [0x95,0xca,0x00,0x80,0x5f,0x48,0xa1,0x92],
};
const SO_UPDATE_CONNECT_CONTEXT: c_int = 0x7010;
const SO_UPDATE_ACCEPT_CONTEXT: c_int = 0x700B;

#[allow(bad_style)]
type LPFN_CONNECTEX = Option<extern "system" fn(
  s: SOCKET,
  name: *const SOCKADDR,
  namelen: c_int,
  lpSendBuffer: PVOID,
  dwSendDataLength: DWORD,
  lpdwBytesSent: LPDWORD,
  lpOverlapped: LPOVERLAPPED,
) -> BOOL>;
#[allow(bad_style)]
type LPFN_ACCEPTEX = Option<extern "system" fn(
  sListenSocket: SOCKET,
  sAcceptSocket: SOCKET,
  lpOutputBuffer: PVOID,
  dwReceiveDataLength: DWORD,
  dwLocalAddressLength: DWORD,
  dwRemoteAddressLength: DWORD,
  lpdwBytesReceived: LPDWORD,
  lpOverlapped: LPOVERLAPPED,
) -> BOOL>;

unsafe fn get_winsock_function(socket: SOCKET, guid: &GUID) -> Result<usize> {
    let mut fn_ptr: usize = 0;
    let mut out_bytes: DWORD = 0;
    if WSAIoctl(
        socket,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        guid as *const GUID as _,
        mem::size_of_val(guid) as DWORD,
        &mut fn_ptr as *mut usize as _,
        mem::size_of::<usize>() as DWORD,
        &mut out_bytes,
        ptr::null_mut(),
        None,
    ) != 0 {
        let err = io::Error::from_raw_os_error(WSAGetLastError());
        error!("WSAIoctl for SIO_GET_EXTENSION_FUNCTION_POINTER failed: {}", err);
        return Err(err);
    }
    if out_bytes as usize != mem::size_of::<usize>() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid length for SIO_GET_EXTENSION_FUNCTION_POINTER"));
    }

    Ok(fn_ptr)
}

fn socket_addr_to_ptrs(addr: &SocketAddr) -> (*const SOCKADDR, c_int) {
    match *addr {
        SocketAddr::V4(ref a) => {
            (a as *const _ as *const _, mem::size_of::<SOCKADDR_IN>() as c_int)
        }
        SocketAddr::V6(ref a) => {
            (a as *const _ as *const _, mem::size_of::<SOCKADDR_IN6_LH>() as c_int)
        }
    }
}