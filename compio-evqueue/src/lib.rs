#[macro_use]
extern crate tracing;

mod evqueue;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
pub mod io_uring;

pub use self::evqueue::EvQueue;
