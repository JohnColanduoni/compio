use crate::EvQueue;

use super::IoUring;

pub trait EvQueueExt {
    /// Obtain an `io_uring` specific event queue interface, if backed by one
    fn as_io_uring(&self) -> Option<&IoUring>;
}

impl EvQueueExt for EvQueue {
    #[inline]
    fn as_io_uring(&self) -> Option<&IoUring> {
        self.0.as_io_uring()
    }
}
