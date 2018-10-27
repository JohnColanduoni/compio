#[macro_use] mod sys;
#[macro_use] mod error;
pub mod channel;

use std::{io, fmt, mem, ptr, slice};
use std::ops::{Deref, DerefMut};
use std::convert::TryFrom;
use std::time::Duration;

use compio_core::os::macos::*;

pub struct Port {
    port: sys::mach_port_name_t,
    has_receive: bool,
    has_send: bool,
}

impl Drop for Port {
    fn drop(&mut self) {
        unsafe {
            if self.has_receive {
                let _ = mach_call!(log: sys::mach_port_mod_refs(sys::mach_task_self(), self.port, sys::MACH_PORT_RIGHT_RECEIVE, -1), "freeing receive right with mach_port_mod_refs failed: {:?}");
            }
            if self.has_send {
                // If the receive right is already dead, this returns
                match sys::mach_port_mod_refs(sys::mach_task_self(), self.port, sys::MACH_PORT_RIGHT_SEND, -1) as u32 {
                    sys::KERN_SUCCESS | sys::KERN_INVALID_RIGHT => (),
                    code => {
                        let err = error::rust_from_mach_error(code as _);
                        error!("freeing send right with mach_port_mod_refs failed: {:?}", err);
                    },
                }
            }
        }
    }
}

pub struct PortMsgBuffer {
    buffer: Vec<u8>,
}

pub struct PortMsg(dyn PortMsgImpl);

trait PortMsgImpl {
    fn as_ptr(&self) -> *const u8;
    fn as_mut_ptr(&mut self) -> *mut u8;

    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    unsafe fn set_len(&mut self, len: usize);
}

impl Port {
    pub fn new() -> io::Result<Port> {
        unsafe {
            let mut port: sys::mach_port_t = 0;
            mach_call!(log: sys::mach_port_allocate(sys::mach_task_self(), sys::MACH_PORT_RIGHT_RECEIVE, &mut port), "mach_port_allocate failed: {:?}")?;
            let port = Port {
                port,
                has_receive: true,
                has_send: false,
            };
            Ok(port)
        }
    }

    pub fn as_raw_port(&self) -> RawPort {
        self.port
    }

    pub fn make_sender(&self) -> io::Result<Port> {
        unsafe {
            let mut port: sys::mach_port_t = 0;
            let mut right: sys::mach_msg_type_name_t = 0;
            mach_call!(log: sys::mach_port_extract_right(sys::mach_task_self(), self.port, sys::MACH_MSG_TYPE_MAKE_SEND, &mut port, &mut right), "mach_port_extract_right failed: {:?}")?;
            if right != sys::MACH_MSG_TYPE_PORT_SEND {
                return Err(io::Error::new(io::ErrorKind::Other, "mach_port_extract_right did not return requested right type"));
            }
            let port = Port {
                port,
                has_receive: false,
                has_send: true,
            };
            Ok(port)
        }
    }

    pub fn send(&self, msg: &mut PortMsg) -> io::Result<()> {
        unsafe {
            msg.header_mut().msgh_remote_port = self.port;
            mach_call!(sys::mach_msg(
                msg.0.as_ptr() as *mut _,
                sys::MACH_SEND_MSG as _,
                msg.header().msgh_size,
                0,
                sys::MACH_PORT_NULL,
                sys::MACH_MSG_TIMEOUT_NONE,
                sys::MACH_PORT_NULL,
            ))?;
            msg.header_mut().msgh_remote_port = sys::MACH_PORT_NULL;
            Ok(())
        }
    }

    pub fn recv(&self, msg: &mut PortMsg, timeout: Option<Duration>) -> io::Result<()> {
        unsafe {
            let mut flags = sys::MACH_RCV_MSG | sys::MACH_RCV_LARGE;
            let mut timeout_arg = sys::MACH_MSG_TIMEOUT_NONE as sys::mach_msg_timeout_t;
            if let Some(duration) = timeout {
                flags |= sys::MACH_RCV_TIMEOUT;
                timeout_arg = duration.as_secs()
                    .checked_mul(1000)
                    .and_then(|x| x.checked_add(duration.subsec_millis() as u64))
                    .and_then(|x| i32::try_from(x).ok())
                    .unwrap_or(std::i32::MAX) as sys::mach_msg_timeout_t;
            }
            mach_call!(sys::mach_msg(
                msg.0.as_mut_ptr() as *mut _,
                flags as _,
                0,
                msg.0.capacity() as _,
                self.port,
                sys::MACH_MSG_TIMEOUT_NONE,
                sys::MACH_PORT_NULL,
            ))?;

            let size = msg.header().msgh_size;
            msg.0.set_len(size as usize);

            Ok(())
        }
    }
}

impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Port")
            .field("port", &format_args!("{:#x?}", self.port))
            .field("has_receive", &self.has_receive)
            .field("has_send", &self.has_send)
            .finish()
    }
}

#[repr(C)]
struct MessageStart {
    header: sys::mach_msg_header_t,
    body: sys::mach_msg_body_t,
}

impl PortMsgBuffer {
    pub fn new() -> PortMsgBuffer {
        // Always keep enough additional capacity around for the trailer, in case we use this buffer for a receive
        let init_len = mem::size_of::<MessageStart>();
        let mut buffer = Vec::with_capacity(init_len + mem::size_of::<sys::mach_msg_trailer_t>());
        unsafe {
            *(buffer.as_mut_ptr() as *mut MessageStart) = MessageStart {
                header: sys::mach_msg_header_t {
                    msgh_bits: sys::MACH_MSG_TYPE_COPY_SEND,
                    msgh_size: mem::size_of::<MessageStart>() as _,
                    msgh_remote_port: sys::MACH_PORT_NULL,
                    msgh_local_port: sys::MACH_PORT_NULL,
                    msgh_voucher_port: sys::MACH_PORT_NULL,
                    msgh_id: 0,
                },
                body: sys::mach_msg_body_t {
                    msgh_descriptor_count: 0,
                },
            };
            buffer.set_len(init_len);
        }
        PortMsgBuffer {
            buffer
        }
    }

    pub fn reset(&mut self) {
        debug_assert!(self.buffer.len() >= mem::size_of::<MessageStart>());
        unsafe {
            self.buffer.set_len(mem::size_of::<MessageStart>());
            *(self.buffer.as_mut_ptr() as *mut MessageStart) = MessageStart {
                header: sys::mach_msg_header_t {
                    msgh_bits: sys::MACH_MSG_TYPE_COPY_SEND,
                    msgh_size: mem::size_of::<MessageStart>() as _,
                    msgh_remote_port: sys::MACH_PORT_NULL,
                    msgh_local_port: sys::MACH_PORT_NULL,
                    msgh_voucher_port: sys::MACH_PORT_NULL,
                    msgh_id: 0,
                },
                body: sys::mach_msg_body_t {
                    msgh_descriptor_count: 0,
                },
            };
        }
    }

    pub fn reserve_inline(&mut self, additional: usize) {
        self.buffer.reserve(additional)
    }

    pub fn extend_inline_data(&mut self, data: &[u8]) {
        self.buffer.reserve(data.len());
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), self.buffer.as_mut_ptr().offset(self.buffer.len() as isize), data.len());
            self.header_mut().msgh_size += data.len() as sys::mach_msg_size_t;
            self.buffer.set_len(self.buffer.len() + data.len());
        }
    }
}

impl PortMsg {
    pub fn inline_data(&self) -> &[u8] {
        if !self.complex() {
            debug_assert!(self.0.len() >= self.header().msgh_size as usize);
            unsafe { slice::from_raw_parts(self.0.as_ptr().offset(mem::size_of::<MessageStart>() as isize), self.header().msgh_size as usize - mem::size_of::<MessageStart>()) }
        } else {
            unimplemented!()
        }
    }

    pub fn inline_data_mut(&mut self) -> &mut [u8] {
        unimplemented!()
    }

    pub fn complex(&self) -> bool {
        self.header().msgh_bits & sys::MACH_MSGH_BITS_COMPLEX != 0
    }

    fn header(&self) -> &sys::mach_msg_header_t {
        debug_assert!(self.0.len() >= mem::size_of::<sys::mach_msg_header_t>());
        unsafe { &*(self.0.as_ptr() as *const sys::mach_msg_header_t) }
    }

    fn header_mut(&mut self) -> &mut sys::mach_msg_header_t {
        debug_assert!(self.0.len() >= mem::size_of::<sys::mach_msg_header_t>());
        unsafe { &mut *(self.0.as_mut_ptr() as *mut sys::mach_msg_header_t) }
    }


}

impl PortMsgImpl for PortMsgBuffer {
    fn as_ptr(&self) -> *const u8 {
        self.buffer.as_ptr()
    }
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buffer.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }
    fn capacity(&self) -> usize {
        self.buffer.capacity()
    }
    unsafe fn set_len(&mut self, len: usize) {
        self.buffer.set_len(len)
    }
}

impl fmt::Debug for PortMsg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "PortMsg {{ ")?;

        write!(f, "header: {{ ")?;
        write!(f, "complex: {:?}, ", self.complex())?;
        write!(f, "size: {:?} ", self.header().msgh_size)?;
        write!(f, "}} ")?;

        write!(f, "inline_data: {:?}", self.inline_data())?;

        write!(f, "}}")?;

        Ok(())
    }
}

impl fmt::Debug for PortMsgBuffer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (&**self).fmt(f)
    }
}

impl Deref for PortMsgBuffer {
    type Target = PortMsg;

    fn deref(&self) -> &PortMsg {
        let gen: &PortMsgImpl = self;
        unsafe { mem::transmute(gen) }
    }
}

impl DerefMut for PortMsgBuffer {
    fn deref_mut(&mut self) -> &mut PortMsg {
        let gen: &mut PortMsgImpl = self;
        unsafe { mem::transmute(gen) }
    }
}