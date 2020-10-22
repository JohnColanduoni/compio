use std::{any::Any, sync::Arc};

pub struct EvQueue(pub(crate) Arc<dyn EvQueueImpl>);

pub(crate) trait EvQueueImpl: Sync + Send + 'static {
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn as_io_uring(&self) -> Option<&crate::io_uring::IoUring> {
        None
    }
}
