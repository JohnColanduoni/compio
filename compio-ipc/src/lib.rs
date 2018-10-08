#![feature(futures_api, async_await, await_macro, pin, arbitrary_self_types)]

#[macro_use]
extern crate log;

#[cfg_attr(unix, path = "platform/unix.rs")]
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::{StreamExt};
    }
}

use std::{io};
use std::future::Future;

use compio_core::queue::EventQueue;

#[derive(Clone, Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;

    use compio_local::LocalExecutor;
    use futures_util::join;

    #[test]
    fn create_stream_pair() {
        let event_queue = EventQueue::new().unwrap();
        let (_a, _b) = Stream::pair(&event_queue).unwrap();
    }

    #[test]
    fn create_prestream_pair() {
        let (_a, _b) = PreStream::pair().unwrap();
    }


    #[test]
    fn simple_send_recv() {
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
}
