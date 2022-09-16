use core::mem::MaybeUninit;

use crate::{riscv::paging::PageTable, vm::PageTableExtension};

#[derive(Debug)]
pub struct ProcessMemory {
    pagetable: PageTable,
    size: usize,
}

impl ProcessMemory {
    pub fn read<T>(&self, addr: usize) -> Option<T> {
        if addr >= self.size || addr + core::mem::size_of::<T>() > self.size {
            return None;
        }

        let mut dst = MaybeUninit::uninit();
        if unsafe { self.pagetable.read(&mut dst, addr).is_err() } {
            return None;
        }

        Some(unsafe { dst.assume_init() })
    }

    #[must_use]
    pub fn write<T: 'static>(&self, addr: usize, value: T) -> bool {
        unsafe { self.pagetable.write(addr, &value).is_ok() }
    }
}

impl Clone for ProcessMemory {
    fn clone(&self) -> Self {
        /*
        let size = self.size;
        if let Err(_) = process
            .context()
            .unwrap()
            .pagetable
            .copy(&mut context_new.pagetable, size)
        {
            return None;
        }
        context_new.sz = process.context().unwrap().sz;
        */
        todo!()
    }
}
