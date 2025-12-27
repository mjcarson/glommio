//! Rust implementations of inlined functions that are no longer exported by
//! liburing by default.

use crate::uring_sys;
use std::ptr;

/// Get the size of our SQE object
const SQE_SIZE: usize = std::mem::size_of::<uring_sys::io_uring_sqe>();

/// Gets a pointer to the next available submission queue entry or null if this queue is full
///
/// # Arguments
///
/// * `ring` - The ring to get the next available submission queue entry from
#[inline]
pub(crate) unsafe fn get_sqe(ring: *mut uring_sys::io_uring) -> *mut uring_sys::io_uring_sqe {
    // get a mutable ref to this rings submission queue
    let sq = &mut (*ring).sq;
    // get the next tail value
    let next = sq.sqe_tail.wrapping_add(1);
    // Check if submission queue is full
    if next.wrapping_sub(sq.sqe_head) > *sq.kring_entries {
        // our submission queue is full so just return null
        return ptr::null_mut();
    }
    // Calculate the index with proper masking
    let index = sq.sqe_tail & *sq.kring_mask;
    // get a pointer to the SQE we are going to write to
    let sqe = sq.sqes.offset(index as isize);
    // make sure this submission queue entry doesn't contain any old data
    ptr::write_bytes(sqe as *mut u8, 0, SQE_SIZE);
    // update our submission queue to point at the next available entry
    sq.sqe_tail = next;
    sqe
}
