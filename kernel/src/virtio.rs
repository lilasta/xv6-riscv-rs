//!
//! virtio device definitions.
//! for both the mmio interface, and virtio descriptors.
//! only tested with qemu.
//! this is the "legacy" virtio interface.
//!
//! the virtio spec:
//! [https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.pdf]
//!

use crate::memory_layout::VIRTIO0;

pub mod disk;

// virtio mmio control registers, mapped starting at 0x10001000.
// from qemu virtio_mmio.h
mod mmio_reg {
    pub const MAGIC_VALUE: usize = 0x000; // 0x74726976
    pub const VERSION: usize = 0x004; // version; 1 is legacy
    pub const DEVICE_ID: usize = 0x008; // device type; 1 is net, 2 is disk
    pub const VENDOR_ID: usize = 0x00c; // 0x554d4551
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const GUEST_PAGE_SIZE: usize = 0x028; // page size for PFN, write-only
    pub const QUEUE_SEL: usize = 0x030; // select queue, write-only
    pub const QUEUE_NUM_MAX: usize = 0x034; // max size of current queue, read-only
    pub const QUEUE_NUM: usize = 0x038; // size of current queue, write-only
    pub const QUEUE_ALIGN: usize = 0x03c; // used ring alignment, write-only
    pub const QUEUE_PFN: usize = 0x040; // physical page number for queue, read/write
    pub const QUEUE_READY: usize = 0x044; // ready bit
    pub const QUEUE_NOTIFY: usize = 0x050; // write-only
    pub const INTERRUPT_STATUS: usize = 0x060; // read-only
    pub const INTERRUPT_ACK: usize = 0x064; // write-only
    pub const STATUS: usize = 0x070; // read/write
}

// status register bits, from qemu virtio_config.h
mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
}

// device feature bits
mod feature {
    pub const BLK_RO: u8 = 5; /* Disk is read-only */
    pub const BLK_SCSI: u8 = 7; /* Supports scsi command passthru */
    pub const BLK_CONFIG_WCE: u8 = 11; /* Writeback mode available in config */
    pub const BLK_MQ: u8 = 12; /* support more than one vq */
    pub const ANY_LAYOUT: u8 = 27;
    pub const RING_INDIRECT_DESC: u8 = 28;
    pub const RING_EVENT_IDX: u8 = 29;
}

mod descriptor {
    // this many virtio descriptors.
    // must be a power of two.
    pub const DESCRIPTOR_NUM: usize = 8;

    // a single descriptor, from the spec.
    #[repr(C)]
    pub struct Descriptor {
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    }

    const VRING_DESC_F_NEXT: u8 = 1; // chained with another descriptor
    const VRING_DESC_F_WRITE: u8 = 2; // device writes (vs read)

    // the (entire) avail ring, from the spec.
    #[repr(C)]
    pub struct Avail {
        flags: u16,                  // always zero
        idx: u16,                    // driver will write ring[idx] next
        ring: [u16; DESCRIPTOR_NUM], // descriptor numbers of chain heads
        unused: u16,
    }

    // one entry in the "used" ring, with which the
    // device tells the driver about completed requests.
    #[repr(C)]
    pub struct UsedElem {
        id: u32, // index of start of completed descriptor chain
        len: u32,
    }

    #[repr(C)]
    pub struct Used {
        flags: u16, // always zero
        idx: u16,   // device increments when it adds a ring[] entry
        ring: [UsedElem; DESCRIPTOR_NUM],
    }

    // these are specific to virtio block devices, e.g. disks,
    // described in Section 5.2 of the spec.
    #[repr(u32)]
    pub enum BlockRequestType {
        Read = 0,
        Write = 1,
    }

    // the format of the first descriptor in a disk request.
    // to be followed by two more descriptors containing
    // the block, and a one-byte status.
    #[repr(C)]
    pub struct BlockRequest {
        ty: BlockRequestType,
        reserved: u32,
        sector: u64,
    }
}

// the address of virtio mmio register r.
pub fn mmio_register(reg: usize) -> *mut u32 {
    <*mut _>::from_bits(VIRTIO0 + reg)
}
