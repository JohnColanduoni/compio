pub mod tcp;

use std::{io};
use std::os::unix::prelude::*;

fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let fd_flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if fd_flags == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_GETFD) failed: {}", err);
            return Err(err);
        }
        if libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_SETFD) failed: {}", err);
            return Err(err);
        }
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_GETFL) failed: {}", err);
            return Err(err);
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            let err = io::Error::last_os_error();
            error!("fcntl(F_SETFL) failed: {}", err);
            return Err(err);
        }

        Ok(())
    }
}
