#[macro_use] extern crate log;

#[cfg_attr(target_os = "windows", path = "platform/windows/mod.rs")]
#[cfg_attr(unix, path = "platform/unix/mod.rs")]
mod platform;

mod tcp;

pub use self::tcp::{TcpListener, TcpStream};