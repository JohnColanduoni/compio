use crate::platform::tcp as platform;

use std::future::Future;
use std::io::{Result, Error};
use std::net::{SocketAddr, Shutdown};

use compio_core::queue::Registrar;

#[derive(Clone, Debug)]
pub struct TcpListener {
    inner: platform::TcpListener,
}

#[derive(Clone, Debug)]
pub struct TcpStream {
    inner: platform::TcpStream,
}

impl TcpListener {
    pub fn bind(addr: &SocketAddr, queue: &Registrar) -> Result<TcpListener> {
        Ok(TcpListener { inner: platform::TcpListener::bind(addr, queue)? })
    }

    pub fn from_std(listener: std::net::TcpListener, queue: &Registrar) -> Result<TcpListener> {
        let inner = platform::TcpListener::from_std(listener, queue)?;
        Ok(TcpListener { inner })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.as_std().local_addr()
    }

    pub fn take_error(&self) -> Result<Option<Error>> {
        self.inner.as_std().take_error()
    }

    pub fn accept<'a>(&'a mut self) -> impl Future<Output = Result<TcpStream>> + Send + 'a {
        async move {
            let inner = await!(self.inner.accept())?;
            Ok(TcpStream { inner })
        }
    }
}

impl TcpStream {
    pub fn connect<'a>(addr: SocketAddr, queue: &'a Registrar) -> impl Future<Output = Result<TcpStream>> + Send + 'a {
        async move {
            let inner = await!(platform::TcpStream::connect(addr, queue))?;
            Ok(TcpStream { inner })
        }
    }

    pub fn from_std(listener: std::net::TcpStream, queue: &Registrar) -> Result<TcpStream> {
        let inner = platform::TcpStream::from_std(listener, queue)?;
        Ok(TcpStream { inner })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.as_std().local_addr()
    }

    pub fn shutdown(&self, how: Shutdown) -> Result<()> {
        self.inner.as_std().shutdown(how)
    }

    pub fn set_ttl(&self, ttl: u32) -> Result<()> {
        self.inner.as_std().set_ttl(ttl)
    }

    pub fn ttl(&self) -> Result<u32> {
        self.inner.as_std().ttl()
    }

    pub fn take_error(&self) -> Result<Option<Error>> {
        self.inner.as_std().take_error()
    }

    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> impl Future<Output = Result<usize>> + Send + 'a {
        self.inner.read(buffer)
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> impl Future<Output = Result<usize>> + Send + 'a {
        self.inner.write(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::str::FromStr;
    use std::time::Duration;
    use std::future::poll_with_tls_waker;

    use compio_local::LocalExecutor;
    use futures_util::{try_join, join};
    use pin_utils::pin_mut;

    #[test]
    fn tcp_listener_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<TcpListener>();
    }

    #[test]
    fn tcp_stream_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}
        is_send_and_sync::<TcpStream>();
    }

    #[test]
    fn connect_echo() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let registrar = executor.registrar();
        executor.block_on(async {
            let mut listener = TcpListener::bind(&SocketAddr::from_str("127.0.0.1:0").unwrap(), &registrar).unwrap();
            let connect = TcpStream::connect(listener.local_addr().unwrap(), &registrar);
            let accept = listener.accept();

            let (mut accepted, mut connected) = try_join!(accept, connect).unwrap();

            let server = async {
                let mut buffer = [0u8; 256];
                let byte_count = await!(accepted.read(&mut buffer)).unwrap();
                let buffer = &buffer[0..byte_count];
                await!(accepted.write(buffer)).unwrap();
            };
            let client = async {
                await!(connected.write(b"Hello World!")).unwrap();
                let mut buffer = [0u8; 256];
                let byte_count = await!(connected.read(&mut buffer)).unwrap();
                let buffer = &buffer[0..byte_count];
                assert_eq!(b"Hello World!", buffer);
            };

            join!(server, client);
        });
    }

    #[test]
    fn connect_echo_ipv6() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let registrar = executor.registrar();
        executor.block_on(async {
            let mut listener = TcpListener::bind(&SocketAddr::from_str("[::1]:0").unwrap(), &registrar).unwrap();
            let connect = TcpStream::connect(listener.local_addr().unwrap(), &registrar);
            let accept = listener.accept();

            let (mut accepted, mut connected) = try_join!(accept, connect).unwrap();

            let server = async {
                let mut buffer = [0u8; 256];
                let byte_count = await!(accepted.read(&mut buffer)).unwrap();
                let buffer = &buffer[0..byte_count];
                await!(accepted.write(buffer)).unwrap();
            };
            let client = async {
                await!(connected.write(b"Hello World!")).unwrap();
                let mut buffer = [0u8; 256];
                let byte_count = await!(connected.read(&mut buffer)).unwrap();
                let buffer = &buffer[0..byte_count];
                assert_eq!(b"Hello World!", buffer);
            };

            join!(server, client);
        });
    }

    #[test]
    fn accept_cancel() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let registrar = executor.registrar();
        executor.block_on(async {
            let mut listener = TcpListener::bind(&SocketAddr::from_str("127.0.0.1:0").unwrap(), &registrar).unwrap();
            let accept = listener.accept();
            pin_mut!(accept);
            assert!(poll_with_tls_waker(accept).is_pending());
        });
        // Make sure cancellation was received by IOCP on Windows
        if cfg!(target_os = "windows") {
            assert_eq!(1, executor.queue().turn(Some(Duration::from_millis(0)), None).unwrap());
        }
    }
}