#[cfg(unix)]
macro_rules! try_libc {
    (fd: $x:expr, $msg:tt $(,)* $($arg:expr),* $(,)*) => {
        {
            let fd = $x;
            if fd < 0 {
                let err = ::std::io::Error::last_os_error();
                error!($msg, err, $($arg,)*);
                return ::std::result::Result::Err(err);
            }
            fd
        }
    };
    ($x:expr, $msg:tt $(,)* $($arg:expr),* $(,)*) => {
        if $x != 0 {
            let err = ::std::io::Error::last_os_error();
            error!($msg, err, $($arg,)*);
            return ::std::result::Result::Err(err);
        }
    };
}

#[cfg(target_os = "linux")]
#[path = "epoll.rs"]
pub(crate) mod queue;

#[cfg(target_os = "macos")]
#[path = "kqueue/mod.rs"]
pub(crate) mod queue;

#[cfg(target_os = "windows")]
#[path = "iocp.rs"]
pub(crate) mod queue;

#[cfg(unix)]
pub(crate) mod unix;

