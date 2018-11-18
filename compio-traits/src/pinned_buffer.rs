use std::{mem};

use cfg_if::cfg_if;

/// A byte buffer that is suitable for zero-copy IO operations
/// 
/// In particular it must provide the following:
///     * It must always provide the same pointer and length (via `bytes`) every time it is called.
///     * It must offer a method of capturing the buffer in a `Box<Any + Send>` in the event that
///         the buffer must outlive the future (e.g. canceled operations on Windows).
pub unsafe trait PinnedBuffer {
    /// Gets the bytes pointer to by this `PinnedBuffer`.
    /// 
    /// This function must always return the same value; failing to do so may result in memory unsafety.
    /// 
    /// Calling this function after [take](PinnedBuffer::take) has been called is not allowed, and may cause
    /// undefined behavior (other than memory unsafety).
    fn bytes(&mut self) -> &[u8];

    /// Captures the memory pointed to by this `PinnedBuffer`, to be released at a later time.
    /// 
    /// After this method is called no more operations are allowed on the `PinnedBuffer`.
    fn take(&mut self) -> Box<Send>;
}

pub unsafe trait PinnedBufferMut: PinnedBuffer {
    fn bytes_mut(&mut self) -> &mut [u8];
}

cfg_if! {
    if #[cfg(feature = "bytes_compat")] {
        unsafe impl PinnedBuffer for bytes::Bytes {
            #[inline]
            fn bytes(&mut self) -> &[u8] {
                // Check if bytes are inline, reallocating if that is the case
                let buffer_pointer = self.as_ptr() as usize;
                let container_pointer = self as *const bytes::Bytes;
                let container_end = unsafe { container_pointer.offset(1) };
                if buffer_pointer >= container_pointer as usize && buffer_pointer < container_end as usize {
                    *self = bytes::Bytes::from(self.to_vec());
                }
                &**self
            }

            fn take(&mut self) -> Box<Send> {
                Box::new(self.clone())
            }
        }

        unsafe impl PinnedBuffer for bytes::BytesMut {
            #[inline]
            fn bytes(&mut self) -> &[u8] {
                // Check if bytes are inline, reallocating if that is the case
                let buffer_pointer = self.as_ptr() as usize;
                let container_pointer = self as *const bytes::BytesMut;
                let container_end = unsafe { container_pointer.offset(1) };
                if buffer_pointer >= container_pointer as usize && buffer_pointer < container_end as usize {
                    *self = bytes::BytesMut::from(self.to_vec());
                }
                &**self
            }

            fn take(&mut self) -> Box<Send> {
                let taken: bytes::BytesMut = mem::replace(self, bytes::BytesMut::new());
                Box::new(taken)
            }
        }

        unsafe impl PinnedBufferMut for bytes::BytesMut {
            #[inline]
            fn bytes_mut(&mut self) -> &mut [u8] {
                // Check if bytes are inline, reallocating if that is the case
                let buffer_pointer = self.as_ptr() as usize;
                let container_pointer = self as *const bytes::BytesMut;
                let container_end = unsafe { container_pointer.offset(1) };
                if buffer_pointer >= container_pointer as usize && buffer_pointer < container_end as usize {
                    *self = bytes::BytesMut::from(self.to_vec());
                }
                &mut **self
            }
        }
    }
}