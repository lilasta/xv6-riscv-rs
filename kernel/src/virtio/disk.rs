use core::{mem::MaybeUninit, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    bitmap::Bitmap,
    lock::{spin::SpinLock, Lock},
    memory_layout::VIRTIO0,
    process,
    virtio::{feature, status},
};

use super::{
    descriptor::{
        Avail, BlockRequest, BlockRequestType, Descriptor, Used, DESCRIPTOR_NUM, VRING_DESC_F_NEXT,
        VRING_DESC_F_WRITE,
    },
    mmio_reg,
};

struct Info {
    buffer: NonNull<dyn Buffer>,
    status: u8,
}

pub struct Disk {
    // a set (not a ring) of DMA descriptors, with which the
    // driver tells the device where to read and write individual
    // disk operations. there are NUM descriptors.
    // most commands consist of a "chain" (a linked list) of a couple of
    // these descriptors.
    descriptor: NonNull<[Descriptor; DESCRIPTOR_NUM]>,

    // a ring in which the driver writes descriptor numbers
    // that the driver would like the device to process.  it only
    // includes the head descriptor of each chain. the ring has
    // NUM elements.
    avail: NonNull<Avail>,

    // a ring in which the device writes descriptor numbers that
    // the device has finished processing (just the head of each chain).
    // there are NUM used ring entries.
    used: NonNull<Used>,

    // our own book-keeping.
    free: Bitmap<DESCRIPTOR_NUM>, // is a descriptor free?
    used_index: usize,            // we've looked this far in used[2..NUM].

    // track info about in-flight operations,
    // for use when completion interrupt arrives.
    // indexed by first descriptor index of chain.
    info: [MaybeUninit<Info>; DESCRIPTOR_NUM],

    // disk command headers.
    // one-for-one with descriptors, for convenience.
    ops: [MaybeUninit<BlockRequest>; DESCRIPTOR_NUM],
}

unsafe fn read_reg(r: usize) -> u32 {
    <*const u32>::from_bits(VIRTIO0 + r).read_volatile()
}

unsafe fn write_reg(r: usize, val: u32) {
    <*mut u32>::from_bits(VIRTIO0 + r).write_volatile(val);
}

impl Disk {
    unsafe fn init() -> Self {
        assert!(read_reg(mmio_reg::MAGIC_VALUE) == 0x74726976);
        assert!(read_reg(mmio_reg::VERSION) == 2);
        assert!(read_reg(mmio_reg::DEVICE_ID) == 2);
        assert!(read_reg(mmio_reg::VENDOR_ID) == 0x554d4551);

        let mut s = 0;
        write_reg(mmio_reg::STATUS, s);

        s |= status::ACKNOWLEDGE;
        write_reg(mmio_reg::STATUS, s);

        s |= status::DRIVER;
        write_reg(mmio_reg::STATUS, s);

        let mut features = read_reg(mmio_reg::DEVICE_FEATURES);
        features &= !(1 << feature::BLK_RO);
        features &= !(1 << feature::BLK_SCSI);
        features &= !(1 << feature::BLK_CONFIG_WCE);
        features &= !(1 << feature::BLK_MQ);
        features &= !(1 << feature::ANY_LAYOUT);
        features &= !(1 << feature::RING_EVENT_IDX);
        features &= !(1 << feature::RING_INDIRECT_DESC);
        write_reg(mmio_reg::DRIVER_FEATURES, features);

        s |= status::FEATURES_OK;
        write_reg(mmio_reg::STATUS, s);

        // re-read status to ensure FEATURES_OK is set.
        s = read_reg(mmio_reg::STATUS);
        assert!(s & status::FEATURES_OK != 0);

        write_reg(mmio_reg::QUEUE_SEL, 0);

        // ensure queue 0 is not in use.
        assert!(read_reg(mmio_reg::QUEUE_READY) == 0);

        let max = read_reg(mmio_reg::QUEUE_NUM_MAX);
        assert!(max != 0);
        assert!(max >= DESCRIPTOR_NUM as u32);

        let descriptor: NonNull<[Descriptor; DESCRIPTOR_NUM]> =
            KernelAllocator::get().allocate().unwrap();
        let avail: NonNull<Avail> = KernelAllocator::get().allocate().unwrap();
        let used: NonNull<Used> = KernelAllocator::get().allocate().unwrap();
        descriptor.as_ptr().write_bytes(0, 1);
        avail.as_ptr().write_bytes(0, 1);
        used.as_ptr().write_bytes(0, 1);

        write_reg(mmio_reg::QUEUE_NUM, DESCRIPTOR_NUM as u32);

        // write physical addresses.
        write_reg(mmio_reg::QUEUE_DESC_LOW, descriptor.addr().get() as u32);
        write_reg(
            mmio_reg::QUEUE_DESC_HIGH,
            (descriptor.addr().get() >> 32) as u32,
        );
        write_reg(mmio_reg::DRIVER_DESC_LOW, avail.addr().get() as u32);
        write_reg(
            mmio_reg::DRIVER_DESC_HIGH,
            (avail.addr().get() >> 32) as u32,
        );
        write_reg(mmio_reg::DEVICE_DESC_LOW, used.addr().get() as u32);
        write_reg(mmio_reg::DEVICE_DESC_HIGH, (used.addr().get() >> 32) as u32);

        // queue is ready.
        write_reg(mmio_reg::QUEUE_READY, 0x1);

        // tell device we're completely ready.
        s |= status::DRIVER_OK;
        write_reg(mmio_reg::STATUS, s);

        Self {
            descriptor,
            avail,
            used,
            free: Bitmap::new(),
            used_index: 0,
            info: MaybeUninit::uninit_array(),
            ops: MaybeUninit::uninit_array(),
        }
    }

    fn allocate_descriptor(&mut self) -> Option<usize> {
        self.free.allocate()
    }

    unsafe fn deallocate_descriptor(&mut self, index: usize) {
        self.free.deallocate(index).unwrap();
        process::wakeup(&*self as *const _ as usize);
    }
}

fn disk() -> &'static SpinLock<Disk> {
    // the virtio driver and device mostly communicate through a set of
    // structures in RAM. pages[] allocates that memory. pages[] is a
    // global (instead of calls to kalloc()) because it must consist of
    // two contiguous pages of page-aligned physical memory.
    static mut DISK: MaybeUninit<SpinLock<Disk>> = MaybeUninit::uninit();
    static INIT: SpinLock<bool> = SpinLock::new(false);

    let mut is_initialized = INIT.lock();
    if !*is_initialized {
        let disk = unsafe { Disk::init() };
        unsafe { DISK.write(SpinLock::new(disk)) };
        *is_initialized = true;
    }

    unsafe { DISK.assume_init_ref() }
}

pub trait Buffer {
    fn block_number(&self) -> usize;
    fn size(&self) -> usize;
    fn addr(&self) -> usize;
    fn start(&mut self);
    fn finish(&mut self);
    fn is_finished(&self) -> bool;
}

unsafe fn rw(mut buffer: NonNull<dyn Buffer>, write: bool) {
    let sector = buffer.as_ref().block_number() * (buffer.as_ref().size() / 512);

    let mut disk = disk().lock();

    let must_allocate_descriptor = |_| loop {
        match disk.allocate_descriptor() {
            Some(desc) => return desc,
            None => process::sleep(&*disk as *const _ as usize, &mut disk),
        }
    };

    let idx = [0; 3].map(must_allocate_descriptor);
    let buf0 = disk.ops[idx[0]].write(BlockRequest {
        ty: match write {
            true => BlockRequestType::Write, // write the disk
            false => BlockRequestType::Read, // read the disk
        },
        reserved: 0,
        sector: sector as u64,
    });

    disk.descriptor.as_mut()[idx[0]] = Descriptor {
        addr: buf0 as *const _ as _,
        len: core::mem::size_of_val(buf0) as u32,
        flags: VRING_DESC_F_NEXT,
        next: idx[1] as u16,
    };

    disk.descriptor.as_mut()[idx[1]] = Descriptor {
        addr: buffer.as_ref().addr() as u64,
        len: buffer.as_ref().size() as u32,
        flags: VRING_DESC_F_NEXT
            | match write {
                true => 0,                   // device reads b->data
                false => VRING_DESC_F_WRITE, // device writes b->data
            },
        next: idx[2] as u16,
    };

    buffer.as_mut().start();
    let info = disk.info[idx[0]].write(Info { buffer, status: 0 });

    disk.descriptor.as_mut()[idx[2]] = Descriptor {
        addr: &mut info.status as *mut _ as u64,
        len: 1,
        flags: VRING_DESC_F_WRITE,
        next: 0,
    };

    disk.avail.as_mut().ring[disk.avail.as_ref().idx as usize % DESCRIPTOR_NUM] = idx[0] as u16;

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    disk.avail.as_mut().idx += 1;
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    write_reg(mmio_reg::QUEUE_NOTIFY, 0); // value is queue number

    // Wait for virtio_disk_intr() to say request has finished.
    while !buffer.as_ref().is_finished() {
        let thin = buffer.as_ptr().cast::<u8>();
        process::sleep(thin.addr(), &mut disk);
    }

    idx.map(|i| disk.deallocate_descriptor(i));
}

pub unsafe fn read(buffer: NonNull<dyn Buffer>) {
    rw(buffer, false);
}

pub unsafe fn write(buffer: NonNull<dyn Buffer>) {
    rw(buffer, true);
}

pub unsafe fn interrupt_handler() {
    let mut disk = disk().lock();

    // the device won't raise another interrupt until we tell it
    // we've seen this interrupt, which the following line does.
    // this may race with the device writing new entries to
    // the "used" ring, in which case we may process the new
    // completion entries in this interrupt, and have nothing to do
    // in the next interrupt, which is harmless.
    write_reg(
        mmio_reg::INTERRUPT_ACK,
        read_reg(mmio_reg::INTERRUPT_STATUS) & 0x3,
    );

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    while disk.used_index != disk.used.as_ref().idx as usize {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        let id = disk.used.as_ref().ring[disk.used_index % DESCRIPTOR_NUM].id;
        assert!(disk.info[id as usize].assume_init_ref().status == 0);

        let mut buf = disk.info[id as usize].assume_init_ref().buffer;
        buf.as_mut().finish();
        process::wakeup(buf.cast::<u8>().addr().get());

        disk.used_index += 1;
    }
}

#[no_mangle]
extern "C" fn virtio_disk_init() {}

#[no_mangle]
extern "C" fn virtio_disk_intr() {
    unsafe { interrupt_handler() }
}

#[repr(C)]
struct BufferC {
    data: [u8; 1024],
    disk: i32,
    dev: u32,
    blockno: u32,
    valid: i32,
}

impl Buffer for BufferC {
    fn block_number(&self) -> usize {
        self.blockno as _
    }

    fn size(&self) -> usize {
        1024
    }

    fn addr(&self) -> usize {
        self.data.as_ptr().addr()
    }

    fn start(&mut self) {
        self.disk = 1;
    }

    fn finish(&mut self) {
        self.disk = 0;
    }

    fn is_finished(&self) -> bool {
        self.disk == 0
    }
}

#[no_mangle]
unsafe extern "C" fn virtio_disk_rw(buf: *mut BufferC, write: i32) {
    let buf = &mut *buf;
    rw(NonNull::new_unchecked(buf), write == 1);
}
