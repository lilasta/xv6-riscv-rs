use core::ffi::c_void;

use crate::riscv::paging::PGSIZE;

use super::descriptor::{Avail, BlockRequest, Descriptor, Used, DESCRIPTOR_NUM};

struct Info {
    b: *mut c_void, // TOD: Buffer
    status: u8,
}

#[repr(align(4096))]
pub struct Disk {
    // the virtio driver and device mostly communicate through a set of
    // structures in RAM. pages[] allocates that memory. pages[] is a
    // global (instead of calls to kalloc()) because it must consist of
    // two contiguous pages of page-aligned physical memory.
    pages: [u8; 2 * PGSIZE],
    // pages[] is divided into three regions (descriptors, avail, and
    // used), as explained in Section 2.6 of the virtio specification
    // for the legacy interface.
    // https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.pdf

    // the first region of pages[] is a set (not a ring) of DMA
    // descriptors, with which the driver tells the device where to read
    // and write individual disk operations. there are NUM descriptors.
    // most commands consist of a "chain" (a linked list) of a couple of
    // these descriptors.
    // points into pages[].
    descriptor: *mut Descriptor,

    // next is a ring in which the driver writes descriptor numbers
    // that the driver would like the device to process.  it only
    // includes the head descriptor of each chain. the ring has
    // NUM elements.
    // points into pages[].
    avail: *mut Avail,

    // finally a ring in which the device writes descriptor numbers that
    // the device has finished processing (just the head of each chain).
    // there are NUM used ring entries.
    // points into pages[].
    used: *mut Used,

    // our own book-keeping.
    free: [u8; DESCRIPTOR_NUM], // is a descriptor free?
    used_index: u16,            // we've looked this far in used[2..NUM].

    // track info about in-flight operations,
    // for use when completion interrupt arrives.
    // indexed by first descriptor index of chain.
    info: [Info; DESCRIPTOR_NUM],

    // disk command headers.
    // one-for-one with descriptors, for convenience.
    ops: [BlockRequest; DESCRIPTOR_NUM],
}
