#![allow(bad_style)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/mach.rs"));

#[inline]
pub fn mach_task_self() -> mach_port_t {
    unsafe { mach_task_self_ }
}

pub const MACH_PORT_DEAD: mach_port_name_t = !0;

pub const MACH_PORT_RIGHT_SEND: mach_port_right_t = 0;
pub const MACH_PORT_RIGHT_RECEIVE: mach_port_right_t = 1;

pub const MACH_MSG_TIMEOUT_NONE: mach_msg_timeout_t = 0;

pub const MACH_PORT_TYPE_SEND: mach_port_type_t = MACH_PORT_TYPE(MACH_PORT_RIGHT_SEND);
pub const MACH_PORT_TYPE_RECEIVE: mach_port_type_t = MACH_PORT_TYPE(MACH_PORT_RIGHT_RECEIVE);

const fn MACH_PORT_TYPE(right: mach_port_right_t) -> mach_port_type_t {
    1 << (right + 16)
}