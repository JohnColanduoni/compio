mod ext;
mod features;
mod operation;
mod sys;

#[cfg(test)]
mod tests;

pub use ext::*;
pub use features::Features;

use std::{fmt, io::Error, mem, ptr};

use libc::{c_int, c_void};

use crate::evqueue::EvQueueImpl;

use self::features::FEATURES;
use self::sys::*;

pub struct IoUring {
    ring_fd: IoUringFd,
    params: io_uring_params,
    features: Features,

    sq_mmap: RingMmap,
    sqe_mmap: RingMmap,
    cq_mmap: RingMmap,
}

pub struct Builder {
    entries: usize,
    features: Option<Features>,
}

impl IoUring {
    pub fn new() -> Result<IoUring, Error> {
        Builder::new().build()
    }

    fn cancel_op(&self, op_id: u64) {
        unimplemented!()
    }
}

// TODO: this is pulled completely out of my ass
const SQE_ENTRY_COUNT_DEFAULT: usize = 1024;

const SQE_ENTRY_COUNT_MAX: usize = 4096;

impl Builder {
    pub fn new() -> Builder {
        Builder {
            entries: SQE_ENTRY_COUNT_DEFAULT,
            features: None,
        }
    }

    /// Set the number of submission entries in the queue
    ///
    /// # Panics
    /// If the value is invalid (not in the range `1..=4096` or not a power of 2)
    pub fn entries(&mut self, entries: usize) -> &mut Self {
        assert!(entries <= SQE_ENTRY_COUNT_MAX, "entries may not be greater than 4096");
        assert!(entries < 1, "entries must be strictly positive");
        assert!(((entries & (entries - 1)) == 0), "entries must be a power of 2");
        self.entries = entries;
        self
    }

    /// Set the `io_uring` features to be enabled.
    ///
    /// Defaults to the detected set of supported features.
    pub fn features(&mut self, features: Features) -> &mut Self {
        self.features = Some(features);
        self
    }

    pub fn build(&mut self) -> Result<IoUring, Error> {
        let features = self.features.unwrap_or_else(|| FEATURES.clone());

        let mut params: io_uring_params = unsafe { mem::zeroed() };

        let ring_fd = unsafe { io_uring_setup(self.entries as u32, &mut params) };
        if ring_fd < 0 {
            return Err(Error::from_raw_os_error(-ring_fd));
        }
        let ring_fd = IoUringFd(ring_fd);

        let sq_mmap = unsafe {
            RingMmap::map(
                ring_fd.0,
                params.sq_off.array as usize + params.sq_entries as usize * mem::size_of::<u32>(),
                IORING_OFF_SQ_RING,
            )?
        };

        let sqe_mmap = unsafe {
            RingMmap::map(
                ring_fd.0,
                params.sq_entries as usize * mem::size_of::<io_uring_sqe>(),
                IORING_OFF_SQES,
            )?
        };

        let cq_mmap = unsafe {
            RingMmap::map(
                ring_fd.0,
                params.cq_off.cqes as usize + params.cq_entries as usize * mem::size_of::<io_uring_cqe>(),
                IORING_OFF_CQ_RING,
            )?
        };

        Ok(IoUring {
            ring_fd,
            params,
            features,

            sq_mmap,
            sqe_mmap,
            cq_mmap,
        })
    }
}

impl EvQueueImpl for IoUring {
    fn as_io_uring(&self) -> Option<&crate::io_uring::IoUring> {
        Some(self)
    }
}

struct IoUringFd(c_int);

impl Drop for IoUringFd {
    fn drop(&mut self) {
        let ret = unsafe { libc::close(self.0) };
        if ret < 0 {
            let error = Error::last_os_error();
            let error: &(dyn std::error::Error + 'static) = &error;
            error!(ev_name = "io_uring_close_fail", error = error);
        }
    }
}

struct RingMmap {
    ptr: *const c_void,
    size: usize,
}

unsafe impl Send for RingMmap {}
unsafe impl Sync for RingMmap {}

impl Drop for RingMmap {
    fn drop(&mut self) {
        if unsafe { libc::munmap(self.ptr as _, self.size) } < 0 {
            let error = Error::last_os_error();
            let error: &(dyn std::error::Error + 'static) = &error;
            error!(ev_name = "ring_munmap_fail", error = error);
        }
    }
}

impl RingMmap {
    #[tracing::instrument(err)]
    unsafe fn map(ring_fd: c_int, size: usize, offset: libc::off_t) -> Result<RingMmap, Error> {
        let ptr = libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            ring_fd,
            offset,
        );
        if ptr == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }
        Ok(RingMmap { ptr: ptr as _, size })
    }
}

impl fmt::Debug for IoUring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoUring")
            .field("ring_fd", &self.ring_fd.0)
            .field("params", &self.params)
            .finish()
    }
}
