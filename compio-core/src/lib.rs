#![feature(futures_api)]
#![cfg_attr(target_os = "windows", feature(pin, arbitrary_self_types))]

#[macro_use] extern crate log;

pub mod queue;
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::unix::{Filter, Registration, RegistrarExt};
    }

    #[cfg(target_os = "windows")]
    pub mod windows {
        pub use crate::platform::queue::{RegistrarExt, Operation, OperationSource};
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub use crate::platform::queue::mach::{RegistrarExt, PortRegistration, RawPort};
    }
}