use libc::{c_int, c_uint, c_void};

cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
        const NR_IO_URING_SETUP: i64 = 425;
        const NR_IO_URING_ENTER: i64 = 426;
        const NR_IO_URING_REGISTER: i64 = 427;
    } else {
        error!("unsupported architecture");
    }
}

pub const IORING_OFF_SQ_RING: libc::off_t = 0;
pub const IORING_OFF_CQ_RING: libc::off_t = 0x8000000;
pub const IORING_OFF_SQES: libc::off_t = 0x10000000;

#[allow(bad_style)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct io_uring_params {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    resv: [u32; 4],
    pub sq_off: io_sqring_offsets,
    pub cq_off: io_cqring_offsets,
}

#[allow(bad_style)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct io_sqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    resv: [u32; 3],
}

#[allow(bad_style)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct io_cqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    resv: [u32; 3],
}

#[allow(bad_style)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct io_uring_cqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

#[allow(bad_style)]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct io_uring_sqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub op_flags: sqe_op_flags,
    pub user_data: u64,
    pub tail: sqe_tail_union,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub union sqe_op_flags {
    pub rw_flags: c_int,
    pub fsync_flags: u32,
    pub poll_events: u16,
    pub poll32_events: u32,
    pub sync_range_flags: u32,
    pub msg_flags: u32,
    pub timeout_flags: u32,
    pub accept_flags: u32,
    pub cancel_flags: u32,
    pub open_flags: u32,
    pub statx_flags: u32,
    pub fadvise_advice: u32,
    pub splice_flags: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub union sqe_tail_union {
    pub data: sqe_tail,
    _pad2: [u64; 3],
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct sqe_tail {
    pub buf_spec: sqe_buf_spec,
    pub personality: u16,
    pub splice_fd_in: i32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub union sqe_buf_spec {
    pub buf_index: u16,
    pub buf_group: u16,
}

pub unsafe fn io_uring_setup(entries: u32, p: *mut io_uring_params) -> c_int {
    libc::syscall(NR_IO_URING_SETUP, entries, p) as c_int
}

pub unsafe fn io_uring_enter(
    fd: c_uint,
    to_submit: c_uint,
    min_complete: c_uint,
    flags: c_uint,
    sig: *mut libc::sigset_t,
) -> c_int {
    libc::syscall(NR_IO_URING_ENTER, fd, to_submit, min_complete, flags, sig) as c_int
}

pub unsafe fn io_uring_register(fd: c_uint, opcode: c_uint, arg: *mut c_void, nr_args: c_uint) -> c_int {
    libc::syscall(NR_IO_URING_REGISTER, fd, opcode, arg, nr_args) as c_int
}
