#[macro_use] mod sys;
#[macro_use] mod error;
pub mod channel;

use std::{io, fmt, mem, ptr, slice, iter};
use std::marker::PhantomData;
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
    capacity_inline: usize,
    capacity_descriptors: usize,
}

impl Drop for PortMsgBuffer {
    fn drop(&mut self) {
        // FIXME: we should deallocate all MOVE ports and memory regions
    }
}

pub struct PortMsg(dyn PortMsgImpl);

#[repr(C)]
pub struct PortMsgDescriptor(sys::mach_msg_type_descriptor_t);

#[repr(C)]
pub struct PortMsgPortDescriptor(sys::mach_msg_port_descriptor_t);

pub enum PortMsgDescriptorKind<'a> {
    Port(&'a PortMsgPortDescriptor),
    // TODO: other subtypes
    Ool(&'a PortMsgDescriptor),
    OolPorts(&'a PortMsgDescriptor),
    OolVolatile(&'a PortMsgDescriptor),
}

pub enum PortMsgDescriptorKindMut<'a> {
    Port(&'a mut PortMsgPortDescriptor),
    // TODO: other subtypes
    Ool(&'a mut PortMsgDescriptor),
    OolPorts(&'a mut PortMsgDescriptor),
    OolVolatile(&'a mut PortMsgDescriptor),
}

trait PortMsgImpl {
    fn as_ptr(&self) -> *const u8;
    fn as_mut_ptr(&mut self) -> *mut u8;

    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    unsafe fn set_len(&mut self, len: usize);

    fn reset_on_send(&mut self);
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortMoveMode {
    Receive,
    Send,
    SendOnce,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortCopyMode {
    Send,
    MakeSend,
    MakeSendOnce,
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

    pub unsafe fn from_raw_port(port: RawPort) -> io::Result<Self> {
        let mut ty: sys::mach_port_type_t = 0;
        mach_call!(log: sys::mach_port_type(sys::mach_task_self(), port, &mut ty), "mach_port_type failed: {:?}")?;
        // TODO: support send-once

        Ok(Port {
            port,
            has_send: ty & sys::MACH_PORT_TYPE_SEND != 0,
            has_receive: ty & sys::MACH_PORT_TYPE_RECEIVE != 0,
        })
    }

    pub fn as_raw_port(&self) -> RawPort {
        self.port
    }

    pub fn into_raw_port(self) -> RawPort {
        let port = self.port;
        mem::forget(self);
        port
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
            let result = mach_call!(sys::mach_msg(
                msg.0.as_ptr() as *mut _,
                sys::MACH_SEND_MSG as _,
                msg.header().msgh_size,
                0,
                sys::MACH_PORT_NULL,
                sys::MACH_MSG_TIMEOUT_NONE,
                sys::MACH_PORT_NULL,
            ));
            msg.header_mut().msgh_remote_port = sys::MACH_PORT_NULL;
            result?;
            msg.0.reset_on_send();
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

// FIXME: a ton of these calculations probably need checked arithmetic in release (e.g. trying to put over 4GB of data in
// a message)
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
            buffer,
            capacity_inline: 0,
            capacity_descriptors: 0,
        }
    }

    /// Resets the [`PortMsgBuffer`], deallocating any owned resources contained within.
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
        // FIXME: we should deallocate all MOVE port rights and memory regions
    }

    #[inline]
    pub fn reserve_inline_data(&mut self, additional: usize) {
        if self.capacity_inline < self.inline_data().len() + additional {
            self.capacity_inline = self.inline_data().len() + additional;
            self.update_reservation();
        }
    }

    #[inline]
    pub fn reserve_descriptors(&mut self, additional: usize) {
        if self.capacity_descriptors < self.descriptors().len() + additional {
            self.capacity_descriptors = self.descriptors().len() + additional;
            self.update_reservation();
        }
    }

    fn update_reservation(&mut self) {
        let total_capacity = mem::size_of::<MessageStart>() + self.capacity_descriptors * mem::size_of::<sys::mach_msg_descriptor_t>() + self.capacity_inline + mem::size_of::<sys::mach_msg_trailer_t>();
        if let Some(additional) = total_capacity.checked_sub(self.buffer.len()) {
            self.buffer.reserve(additional);
        }
    }

    #[inline]
    pub fn extend_inline_data(&mut self, data: &[u8]) {
        // Ensure we maintain space for the trailer
        let final_inline_len = self.inline_data().len() + data.len();
        if final_inline_len > self.capacity_inline {
            self.capacity_inline = final_inline_len;
            self.update_reservation();
        }
        unsafe {
            debug_assert!(self.buffer.capacity() - self.buffer.len() >= data.len());
            ptr::copy_nonoverlapping(data.as_ptr(), self.buffer.as_mut_ptr().offset(self.buffer.len() as isize), data.len());
            self.header_mut().msgh_size += data.len() as sys::mach_msg_size_t;
            self.buffer.set_len(self.buffer.len() + data.len());
        }
    }

    /// Attaches a port to a message, marking for the designated right to be copied on transmission.
    /// 
    /// It is the responsibility of the caller to ensure that the port lives until the message is sent or the port is removed
    /// from the message.
    pub unsafe fn copy_right(&mut self, mode: PortCopyMode, port: &Port) {
        self.copy_right_raw(mode, port.as_raw_port())
    }

    /// Attaches a port to a message, marking for the designated right to be copied on transmission.
    /// 
    /// It is the responsibility of the caller to ensure that the port lives until the message is sent or the port is removed
    /// from the message.
    pub unsafe fn copy_right_raw(&mut self, mode: PortCopyMode, port: RawPort) {
        let mut descriptor = sys::mach_msg_port_descriptor_t {
            name: port,
            pad1: 0,
            _bitfield_1: mem::zeroed(),
        };
        descriptor.set_type(sys::MACH_MSG_PORT_DESCRIPTOR);
        descriptor.set_disposition(match mode {
            PortCopyMode::Send => sys::MACH_MSG_TYPE_COPY_SEND,
            PortCopyMode::MakeSend => sys::MACH_MSG_TYPE_MAKE_SEND,
            PortCopyMode::MakeSendOnce => sys::MACH_MSG_TYPE_MAKE_SEND_ONCE,
        });
        self.append_descriptor(descriptor);
    }

    /// Attaches a port to a message, marking for the designated right to be moved on transmission.
    pub fn move_right(&mut self, mode: PortMoveMode, port: Port) {
        unsafe { self.move_right_raw(mode, port.into_raw_port()) }
    }

    /// Attaches a port to a message, marking for the designated right to be moved on transmission.
    pub unsafe fn move_right_raw(&mut self, mode: PortMoveMode, port: RawPort) {
        let mut descriptor = sys::mach_msg_port_descriptor_t {
            name: port,
            pad1: 0,
            _bitfield_1: mem::zeroed(),
        };
        descriptor.set_type(sys::MACH_MSG_PORT_DESCRIPTOR);
        descriptor.set_disposition(match mode {
            PortMoveMode::Receive => sys::MACH_MSG_TYPE_MOVE_RECEIVE,
            PortMoveMode::Send => sys::MACH_MSG_TYPE_MOVE_SEND,
            PortMoveMode::SendOnce => sys::MACH_MSG_TYPE_MOVE_SEND_ONCE,
        });
        self.append_descriptor(descriptor);
    }

    unsafe fn append_descriptor<T>(&mut self, descriptor: T) {
        // TODO: special case when there is no inline data to be shuffled?
        debug_assert!(mem::size_of::<T>() <= mem::size_of::<sys::mach_msg_descriptor_t>());
        let descriptor_bytes = slice::from_raw_parts(&descriptor as *const T as *const u8, mem::size_of::<T>());
        let insertion_offset = mem::size_of::<MessageStart>() + self.descriptors_byte_len();
        self.buffer.splice(insertion_offset..insertion_offset, descriptor_bytes.iter().cloned());
        *self.descriptor_count_mut() += 1;
        self.header_mut().msgh_bits |= sys::MACH_MSGH_BITS_COMPLEX;
        self.header_mut().msgh_size += mem::size_of::<T>() as sys::mach_msg_size_t;
        // Update reservations
        if self.descriptor_count() as usize > self.capacity_descriptors {
            self.capacity_descriptors = self.descriptor_count() as usize;
            self.update_reservation();
        }
    }
}

impl PortMsg {
    #[inline]
    pub fn inline_data(&self) -> &[u8] {
        debug_assert!(self.0.len() >= self.header().msgh_size as usize);
        let offset = mem::size_of::<MessageStart>() + self.descriptors_byte_len();
        unsafe { slice::from_raw_parts(self.0.as_ptr().offset(offset as isize), self.header().msgh_size as usize - offset) }
    }

    #[inline]
    pub fn inline_data_mut(&mut self) -> &mut [u8] {
        debug_assert!(self.0.len() >= self.header().msgh_size as usize);
        let offset = mem::size_of::<MessageStart>() + self.descriptors_byte_len();
        unsafe { slice::from_raw_parts_mut(self.0.as_mut_ptr().offset(offset as isize), self.header().msgh_size as usize - offset) }
    }

    #[inline]
    pub fn descriptors(&self) -> PortMsgDescriptorIter {
        PortMsgDescriptorIter {
           rem_count: self.descriptor_count(),
           ptr: unsafe { self.0.as_ptr().add(mem::size_of::<MessageStart>()) as *const PortMsgDescriptor },
           msg: PhantomData,
        }
    }

    #[inline]
    pub fn descriptors_mut(&mut self) -> PortMsgDescriptorIterMut {
        PortMsgDescriptorIterMut {
           rem_count: self.descriptor_count(),
           ptr: unsafe { self.0.as_mut_ptr().add(mem::size_of::<MessageStart>()) as *mut PortMsgDescriptor },
           msg: PhantomData,
        }
    }

    #[inline]
    pub fn descriptor_count(&self) -> usize {
        unsafe { (*(self.0.as_ptr() as *const MessageStart)).body.msgh_descriptor_count as usize }
    }

    #[inline]
    unsafe fn descriptor_count_mut(&mut self) -> &mut sys::mach_msg_size_t {
        &mut (*(self.0.as_mut_ptr() as *mut MessageStart)).body.msgh_descriptor_count
    }

    #[inline]
    fn descriptors_byte_len(&self) -> usize {
        let mut iter = self.descriptors();
        let start_ptr = iter.ptr;
        while let Some(_) = iter.next() {
        }
        iter.ptr as usize - start_ptr as usize
    }

    #[inline]
    pub fn complex(&self) -> bool {
        self.header().msgh_bits & sys::MACH_MSGH_BITS_COMPLEX != 0
    }

    #[inline]
    fn header(&self) -> &sys::mach_msg_header_t {
        debug_assert!(self.0.len() >= mem::size_of::<sys::mach_msg_header_t>());
        unsafe { &*(self.0.as_ptr() as *const sys::mach_msg_header_t) }
    }

    #[inline]
    fn header_mut(&mut self) -> &mut sys::mach_msg_header_t {
        debug_assert!(self.0.len() >= mem::size_of::<sys::mach_msg_header_t>());
        unsafe { &mut *(self.0.as_mut_ptr() as *mut sys::mach_msg_header_t) }
    }
}

impl PortMsgDescriptor {
    #[inline]
    pub fn kind(&self) -> PortMsgDescriptorKind {
        match self.0.type_() {
            sys::MACH_MSG_PORT_DESCRIPTOR => PortMsgDescriptorKind::Port(unsafe { &*(self as *const _ as *const PortMsgPortDescriptor) }),
            sys::MACH_MSG_OOL_DESCRIPTOR => PortMsgDescriptorKind::Ool(unsafe { &*(self as *const _ as *const PortMsgDescriptor) }),
            sys::MACH_MSG_OOL_PORTS_DESCRIPTOR => PortMsgDescriptorKind::OolPorts(unsafe { &*(self as *const _ as *const PortMsgDescriptor) }),
            sys::MACH_MSG_OOL_VOLATILE_DESCRIPTOR => PortMsgDescriptorKind::OolVolatile(unsafe { &*(self as *const _ as *const PortMsgDescriptor) }),
            _ => unreachable!(), 
        }
    }

    #[inline]
    pub fn kind_mut(&mut self) -> PortMsgDescriptorKindMut {
        match self.0.type_() {
            sys::MACH_MSG_PORT_DESCRIPTOR => PortMsgDescriptorKindMut::Port(unsafe { &mut *(self as *mut _ as *mut PortMsgPortDescriptor) }),
            sys::MACH_MSG_OOL_DESCRIPTOR => PortMsgDescriptorKindMut::Ool(unsafe { &mut *(self as *mut _ as *mut PortMsgDescriptor) }),
            sys::MACH_MSG_OOL_PORTS_DESCRIPTOR => PortMsgDescriptorKindMut::OolPorts(unsafe { &mut *(self as *mut _ as *mut PortMsgDescriptor) }),
            sys::MACH_MSG_OOL_VOLATILE_DESCRIPTOR => PortMsgDescriptorKindMut::OolVolatile(unsafe { &mut *(self as *mut _ as *mut PortMsgDescriptor) }),
            _ => unreachable!(), 
        }
    }

    #[inline]
    fn size(&self) -> usize {
        match self.0.type_() {
            sys::MACH_MSG_PORT_DESCRIPTOR => mem::size_of::<sys::mach_msg_port_descriptor_t>(),
            sys::MACH_MSG_OOL_DESCRIPTOR => mem::size_of::<sys::mach_msg_ool_descriptor_t>(),
            sys::MACH_MSG_OOL_PORTS_DESCRIPTOR => mem::size_of::<sys::mach_msg_ool_ports_descriptor_t>(),
            sys::MACH_MSG_OOL_VOLATILE_DESCRIPTOR => mem::size_of::<sys::mach_msg_ool_descriptor_t>(),
            _ => unreachable!(),
        }
    }

}

impl PortMsgPortDescriptor {
    #[inline]
    pub fn take_port(&mut self) -> io::Result<Option<Port>> {
        if let Some(port) = self.take_raw_port() {
            Ok(Some(unsafe { Port::from_raw_port(port)? }))
        } else {
            Ok(None)
        }
    }

    #[inline]
    pub fn take_raw_port(&mut self) -> Option<RawPort> {
        if self.0.name == sys::MACH_PORT_NULL || self.0.name == sys::MACH_PORT_DEAD {
            return None;
        }
        Some(mem::replace(&mut self.0.name, sys::MACH_PORT_NULL))
    }
}

impl Deref for PortMsgPortDescriptor {
    type Target = PortMsgDescriptor;

    #[inline]
    fn deref(&self) -> &PortMsgDescriptor {
        unsafe { &* { self as *const _ as *const PortMsgDescriptor } }
    }
}

pub struct PortMsgDescriptorIter<'a> {
    msg: PhantomData<&'a PortMsg>,
    ptr: *const PortMsgDescriptor,
    rem_count: usize,
}

impl<'a> Iterator for PortMsgDescriptorIter<'a> {
    type Item = &'a PortMsgDescriptor;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(new_count) = self.rem_count.checked_sub(1) {
            self.rem_count = new_count;
            unsafe {
                let current = &*self.ptr;
                self.ptr = (self.ptr as *const u8).add(current.size()) as *const PortMsgDescriptor;
                Some(current)
            }
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.rem_count, Some(self.rem_count))
    }

    #[inline]
    fn count(self) -> usize {
        self.rem_count
    }
}

impl<'a> ExactSizeIterator for PortMsgDescriptorIter<'a> {
}

pub struct PortMsgDescriptorIterMut<'a> {
    msg: PhantomData<&'a PortMsg>,
    ptr: *mut PortMsgDescriptor,
    rem_count: usize,
}

impl<'a> Iterator for PortMsgDescriptorIterMut<'a> {
    type Item = &'a mut PortMsgDescriptor;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(new_count) = self.rem_count.checked_sub(1) {
            self.rem_count = new_count;
            unsafe {
                let current = &mut *self.ptr;
                self.ptr = (self.ptr as *mut u8).add(current.size()) as *mut PortMsgDescriptor;
                Some(current)
            }
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.rem_count, Some(self.rem_count))
    }

    #[inline]
    fn count(self) -> usize {
        self.rem_count
    }
}

impl<'a> ExactSizeIterator for PortMsgDescriptorIterMut<'a> {
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

    fn reset_on_send(&mut self) {
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
            // FIXME: keep resources marked as copied?
        }
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