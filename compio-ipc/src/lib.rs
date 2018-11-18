#![feature(futures_api, async_await, await_macro, pin, arbitrary_self_types)]
#![feature(existential_type)]
#![cfg_attr(unix, feature(min_const_fn))]
#![cfg_attr(target_os = "macos", feature(try_from))]
#![cfg_attr(test, feature(gen_future))]

#[macro_use]
extern crate log;

#[cfg_attr(unix, path = "platform/unix/mod.rs")]
#[cfg_attr(target_os = "windows", path = "platform/windows.rs")]
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::{StreamExt};
    }

    #[cfg(target_os = "windows")]
    pub mod windows {
        pub use crate::platform::{StreamExt};
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub use crate::platform::mach::{Port, PortMsg, PortMsgBuffer};
    }
}

use std::{io, fmt};
use std::future::Future;

use compio_core::queue::Registrar;
use compio_traits::{AsyncRead, AsyncWrite, PinnedBuffer, PinnedBufferMut};

#[derive(Clone)]
pub struct Stream {
    inner: platform::Stream,
}

/**
 * A [Stream](crate::Stream) that has not yet been been associated with a [EventQueue](compio_core::queue::EventQueue).
 * 
 * This is useful on platforms where the underlying primitive cannot be unregistered from the [EventQueue](compio_core::queue::EventQueue)
 * (e.g. Windows).
 */
pub struct PreStream {
    inner: platform::PreStream,
}

#[derive(Clone)]
pub struct Channel {
    inner: platform::Channel,
}

/**
 * A [Channel](crate::Channel) that has not yet been been associated with a [EventQueue](compio_core::queue::EventQueue).
 * 
 * This is useful on platforms where the underlying primitive cannot be unregistered from the [EventQueue](compio_core::queue::EventQueue)
 * (e.g. Windows).
 */
pub struct PreChannel {
    inner: platform::PreChannel,
}

impl Stream {
    pub fn pair(queue: &Registrar) -> io::Result<(Stream, Stream)> {
        let (a, b) = PreStream::pair()?;
        Ok((
            a.register(queue)?,
            b.register(queue)?,
        ))
    }
}

impl<'a> AsyncRead<'a> for Stream {
    type Read = platform::StreamReadFuture<'a>;
    type ReadZeroCopy = platform::StreamReadFuture<'a>;

    fn read(&'a mut self, buffer: &'a mut [u8]) -> Self::Read {
        self.inner.read(buffer)
    }

    fn read_zero_copy<B: PinnedBufferMut>(&'a mut self, buffer: &'a mut B) -> Self::ReadZeroCopy {
        // TODO: windows implementation
        self.inner.read(buffer.bytes_mut())
    }
}

impl<'a> AsyncWrite<'a> for Stream {
    type Write = platform::StreamWriteFuture<'a>;
    type WriteZeroCopy = platform::StreamWriteFuture<'a>;

    fn write(&'a mut self, buffer: &'a [u8]) -> Self::Write {
        self.inner.write(buffer)
    }

    fn write_zero_copy<B: PinnedBuffer>(&'a mut self, buffer: &'a mut B) -> Self::WriteZeroCopy {
        // TODO: windows implementation
        self.inner.write(buffer.bytes())
    }
}


impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl PreStream {
    pub fn register(self, queue: &Registrar) -> io::Result<Stream> {
        Ok(Stream {
            inner: self.inner.register(queue)?,
        })
    }

    pub fn pair() -> io::Result<(PreStream, PreStream)> {
        let (a, b) = platform::PreStream::pair()?;
        Ok((
            PreStream { inner: a, },
            PreStream { inner: b, },
        ))
    }
}

impl fmt::Debug for PreStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}


impl Channel {
    pub fn pair(queue: &Registrar) -> io::Result<(Channel, Channel)> {
        let (a, b) = PreChannel::pair()?;
        Ok((
            a.register(queue)?,
            b.register(queue)?,
        ))
    }

    pub fn recv<'a>(&'a mut self, buffer: &'a mut [u8]) -> impl Future<Output=io::Result<usize>> + Send + 'a {
        self.inner.recv(buffer)
    }

    pub fn send<'a>(&'a mut self, buffer: &'a [u8]) -> impl Future<Output=io::Result<()>> + Send + 'a {
        self.inner.send(buffer)
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl PreChannel {
    pub fn register(self, queue: &Registrar) -> io::Result<Channel> {
        Ok(Channel {
            inner: self.inner.register(queue)?,
        })
    }

    pub fn pair() -> io::Result<(PreChannel, PreChannel)> {
        let (a, b) = platform::PreChannel::pair()?;
        Ok((
            PreChannel { inner: a, },
            PreChannel { inner: b, },
        ))
    }
}

impl fmt::Debug for PreChannel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{mem};
    use std::future::poll_with_tls_waker;
    use std::time::Duration;

    use compio_core::queue::EventQueue;
    use compio_local::LocalExecutor;
    use futures_util::join;
    use pin_utils::pin_mut;

    #[test]
    fn stream_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Stream>();
    }

    #[test]
    fn prestream_is_send() {
        fn is_send<T: Send>() {}
        is_send::<PreStream>();
    }

    #[test]
    fn create_stream_pair() {
        let _ = env_logger::try_init();

        let event_queue = EventQueue::new().unwrap();
        let (_a, _b) = Stream::pair(&event_queue.registrar()).unwrap();
    }

    #[test]
    fn create_prestream_pair() {
        let _ = env_logger::try_init();

        let (_a, _b) = PreStream::pair().unwrap();
    }


    #[test]
    fn stream_simple_send_recv() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut b) = Stream::pair(&executor.registrar()).unwrap(); 

        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.read(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                buffer
            };
            let write = async {
                await!(b.write(b"Hello World!")).unwrap()
            };
            let (read, _write) = join!(read, write);

            assert_eq!(b"Hello World!", &*read);
        });
    }

    #[test]
    fn stream_recv_cancel() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut _b) = Stream::pair(&executor.registrar()).unwrap(); 

        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.read(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                buffer
            };
            pin_mut!(read);
            assert!(poll_with_tls_waker(read).is_pending());
        });
        // Make sure the IOCP received the cancellation notification
        if cfg!(target_os = "windows") {
            assert_eq!(1, executor.queue().turn(Some(Duration::from_millis(0)), None).unwrap());
        }
    }

    #[test]
    fn channel_simple_send_recv() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut b) = Channel::pair(&executor.registrar()).unwrap(); 

        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.recv(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                buffer
            };
            let write = async {
                await!(b.send(b"Hello World!")).unwrap()
            };
            let (read, _write) = join!(read, write);

            assert_eq!(b"Hello World!", &*read);
        });
    }

    #[test]
    fn channel_boundary_send_recv() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut b) = Channel::pair(&executor.registrar()).unwrap(); 

        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.recv(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                let mut buffer2 = vec![0u8; 64];
                let byte_count2 = await!(a.recv(&mut buffer2)).unwrap();
                buffer2.truncate(byte_count2);
                (buffer, buffer2)
            };
            let write = async move {
                await!(b.send(b"Hello World!!")).unwrap();
                await!(b.send(b":)")).unwrap();
                mem::drop(b);
            };
            let (_write, read) = join!(write, read);

            assert_eq!(b"Hello World!!", &*read.0);
            assert_eq!(b":)", &*read.1);
        });
    }

    #[test]
    fn channel_recv_cancel() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut _b) = Channel::pair(&executor.registrar()).unwrap(); 

        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.recv(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                buffer
            };
            pin_mut!(read);
            assert!(poll_with_tls_waker(read).is_pending());
        });
        // Make sure the IOCP received the cancellation notification
        if cfg!(target_os = "windows") {
            assert_eq!(1, executor.queue().turn(Some(Duration::from_millis(0)), None).unwrap());
        }
    }
}
