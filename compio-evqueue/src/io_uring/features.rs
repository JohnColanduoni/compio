use std::ptr;

use lazy_static::lazy_static;

use super::sys::io_uring_setup;

lazy_static! {
    pub(crate) static ref FEATURES: Features = Features::test();
}

#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub io_uring: bool,
}

impl Features {
    pub fn test() -> Features {
        let io_uring = unsafe { io_uring_setup(0, ptr::null_mut()) != -libc::ENOSYS };

        Features { io_uring }
    }
}
