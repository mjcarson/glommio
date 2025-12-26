// In glommio/src/uring_sys/inline_helpers.rs or similar

use crate::uring_sys;
use std::ptr;
use std::sync::atomic::{fence, Ordering};

/// Reimplementation of io_uring_get_sqe for liburing 2.2+
#[inline]
pub(crate) unsafe fn get_sqe(ring: *mut uring_sys::io_uring) -> *mut uring_sys::io_uring_sqe {
    let sq = &mut (*ring).sq;
    let next = sq.sqe_tail;
    // Get current head - different for SQPOLL vs normal mode
    let head = if (*ring).flags & uring_sys::IORING_SETUP_SQPOLL != 0 {
        // In SQPOLL mode, the kernel thread updates khead
        fence(Ordering::Acquire);
        ptr::read_volatile(sq.khead)
    } else {
        // In normal mode, we track it ourselves
        sq.sqe_head
    };
    // Check if queue is full
    if next - head >= *sq.kring_entries {
        return ptr::null_mut();
    }
    // Calculate index and get SQE
    let index = next & *sq.kring_mask;
    let sqe = sq.sqes.offset(index as isize);
    // Update tail
    sq.sqe_tail = next.wrapping_add(1);
    sqe
}
