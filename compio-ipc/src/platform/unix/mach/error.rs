use super::sys;

use std::{io, fmt};
use std::ffi::CStr;

macro_rules! mach_call {
    (log: $x:expr, $fmt_str:tt $(, $fmt_arg:expr $(,)*)* ) => {
        match mach_call!($x) {
            Ok(()) => Ok(()),
            Err(err) => {
                error!($fmt_str, err, $($fmt_arg,)* );
                Err(err)
            }
        }
    };
    ($x:expr) => {
        match $x {
            0 => Ok(()),
            code => {
                let err = $crate::platform::mach::error::rust_from_mach_error(code);
                Err(err)
            }
        }
    };
}

pub fn rust_from_mach_error(code: sys::mach_error_t) -> io::Error {
    // TODO: transfer more equivalent codes to io::ErrorKind
    let kind = match code as u32 {
        sys::MACH_SEND_TIMED_OUT=> io::ErrorKind::TimedOut,
        sys::MACH_RCV_TIMED_OUT=> io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, ErrorWrapper(code))
}


// Struct that wraps a mach error code for placement inside a std::io::Error
struct ErrorWrapper(sys::mach_error_t);

impl fmt::Debug for ErrorWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = unsafe { CStr::from_ptr(sys::mach_error_string(self.0)) };
        write!(f, "MachError {{ code: {:#x?}, description: {:?} }}", self.0, name)
    }
}

impl fmt::Display for ErrorWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = unsafe { CStr::from_ptr(sys::mach_error_string(self.0)) };
        write!(f, "{:?}", name)
    }
}

impl std::error::Error for ErrorWrapper {

}