mod pinned_buffer;

pub use self::pinned_buffer::{PinnedBuffer, PinnedBufferMut};

use std::{io};
use std::future::Future;

/// A trait that abstracts a writable asynchronous stream
pub trait AsyncWrite<'a> {
    type Write: Future<Output = io::Result<usize>> + Send + 'a;
    type WriteZeroCopy: Future<Output = io::Result<usize>> + Send + 'a;

    fn write(&'a mut self, buffer: &'a [u8]) -> Self::Write;

    /// Performs an asynchronous write to the stream, possibly enabling zero-copy optimiations
    /// 
    /// To enable zero-copy operations on all platforms, the implementation may require the ability to make
    /// the buffer outlive the [Future](Future) representing the operation. This is the case, for example, when 
    /// an operation is cancelled on Windows. The buffer passed to this function must implement [PinnedBuffer](PinnedBuffer)
    /// which includes a [take](PinnedBuffer::take) function, which must return a sentinel that ensures the memory referenced by the buffer is
    /// valid until it is dropped.
    /// 
    /// Callers must be able to deal with [take](PinnedBuffer::take) function being called at any time between this function being called, and the
    /// future either completing or being dropped (whichever comes first).
    fn write_zero_copy<B: PinnedBuffer>(&'a mut self, buffer: &'a mut B) -> Self::WriteZeroCopy;
}

/// A trait that abstracts a readable asynchronous stream
pub trait AsyncRead<'a> {
    type Read: Future<Output = io::Result<usize>> + Send + 'a;
    type ReadZeroCopy: Future<Output = io::Result<usize>> + Send + 'a;

    fn read(&'a mut self, buffer: &'a mut [u8]) -> Self::Read;

    /// Performs an asynchronous read to the stream, possibly enabling zero-copy optimiations
    /// 
    /// To enable zero-copy operations on all platforms, the implementation may require the ability to make
    /// the buffer outlive the [Future](Future) representing the operation. This is the case, for example, when 
    /// an operation is cancelled on Windows. The buffer passed to this function must implement [PinnedBuffer](PinnedBuffer)
    /// which includes a [take](PinnedBuffer::take) function, which must return a sentinel that ensures the memory referenced by the buffer is
    /// valid until it is dropped.
    /// 
    /// Callers must be able to deal with [take](PinnedBuffer::take) function being called at any time between this function being called, and the
    /// future either completing or being dropped (whichever comes first).
    fn read_zero_copy<B: PinnedBufferMut>(&'a mut self, buffer: &'a mut B) -> Self::ReadZeroCopy;
}
