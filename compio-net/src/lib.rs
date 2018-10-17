#![feature(futures_api, async_await, await_macro, pin, arbitrary_self_types)]
#![cfg_attr(test, feature(gen_future))]

#[macro_use] extern crate log;

#[cfg_attr(target_os = "windows", path = "platform/windows/mod.rs")]
mod platform;

mod tcp;

pub use self::tcp::{TcpListener, TcpStream};