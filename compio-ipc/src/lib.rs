#![feature(futures_api, async_await, await_macro, pin, arbitrary_self_types)]
#![cfg_attr(test, feature(gen_future))]

#[macro_use]
extern crate log;

#[cfg_attr(unix, path = "platform/unix.rs")]
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
}

use std::{io, fmt};
use std::future::Future;

use compio_core::queue::EventQueue;

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

impl Stream {
    pub fn pair(event_queue: &EventQueue) -> io::Result<(Stream, Stream)> {
        let (a, b) = PreStream::pair()?;
        Ok((
            a.register(event_queue)?,
            b.register(event_queue)?,
        ))
    }

    pub fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> impl Future<Output=io::Result<usize>> + Send + 'a {
        self.inner.read(buffer)
    }

    pub fn write<'a>(&'a mut self, buffer: &'a [u8]) -> impl Future<Output=io::Result<usize>> + Send + 'a {
        self.inner.write(buffer)
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl PreStream {
    pub fn register(self, event_queue: &EventQueue) -> io::Result<Stream> {
        Ok(Stream {
            inner: self.inner.register(event_queue)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::poll_with_tls_waker;
    use std::time::Duration;

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
        let (_a, _b) = Stream::pair(&event_queue).unwrap();
    }

    #[test]
    fn create_prestream_pair() {
        let _ = env_logger::try_init();

        let (_a, _b) = PreStream::pair().unwrap();
    }


    #[test]
    fn simple_send_recv() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut b) = Stream::pair(executor.queue()).unwrap(); 

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
    fn recv_cancel() {
        let _ = env_logger::try_init();

        let mut executor = LocalExecutor::new().unwrap();
        let (mut a, mut _b) = Stream::pair(executor.queue()).unwrap(); 

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
        assert_eq!(1, executor.queue().turn(Some(Duration::from_millis(0)), None).unwrap());
    }
}
