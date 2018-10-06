#![feature(futures_api)]

#[macro_use] extern crate log;

pub mod queue;
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::unix::{Filter, Registration, EventQueueExt};
    }

    #[cfg(target_os = "macos")]
    pub mod kqueue {
        pub use crate::platform::queue::{EventQueueExt};
    }
}