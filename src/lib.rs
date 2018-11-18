pub use compio_core::queue as queue;
pub use compio_traits::*;
pub use compio_local as local;
pub use compio_threadpool as threadpool;
pub use compio_ipc as ipc;
pub use compio_net as net;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub mod prelude {
            pub use compio_core::os::unix::{RegistrarExt};
            pub use compio_ipc::os::unix::{StreamExt};
        }
        pub use compio_core::os::unix as core;
        pub use compio_ipc::os::unix as ipc;
    }

    #[cfg(target_os = "windows")]
    pub mod windows {
        pub mod prelude {
            pub use compio_core::os::windows::{RegistrarExt};
            pub use compio_ipc::os::windows::{StreamExt};
        }
        pub use compio_core::os::windows as core;
        pub use compio_ipc::os::windows as ipc;
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub mod prelude {

        }

        pub use compio_core::os::macos as core;
        pub use compio_ipc::os::macos as ipc;
    }
}