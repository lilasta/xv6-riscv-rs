use core::ffi::{c_char, CStr};

use crate::{
    config::MAXARG,
    elf::{ELFHeader, ProgramHeader},
    fs::{InodeLockGuard, InodeOps},
    log,
    process::{allocate_pagetable, free_pagetable, process::ProcessContext},
    riscv::paging::{pg_roundup, PageTable, PGSIZE},
    vm::binding::copyout,
};

unsafe fn ptr_to_slice<'a, T>(start: *const *const T) -> &'a [*const T] {
    let mut ptr = start;
    while !(*ptr).is_null() {
        ptr = ptr.offset(1);
    }
    core::slice::from_ptr_range(start..ptr)
}

fn load_seg(
    pagetable: &mut PageTable,
    va: usize,
    inode: &InodeLockGuard,
    offset: usize,
    size: usize,
) -> bool {
    for i in (0..size).step_by(PGSIZE) {
        let pa = pagetable.virtual_to_physical(va + i).unwrap();
        let n = (size - i).min(PGSIZE);
        if inode
            .copy_to(<*mut u8>::from_bits(pa), offset + i, n)
            .is_err()
        {
            return false;
        }
    }
    true
}

pub unsafe fn execute(
    current_context: &mut ProcessContext,
    path: *const c_char,
    argv: *const *const c_char,
) -> i32 {
    let path = CStr::from_ptr(path).to_str().unwrap();

    let log = log::start();
    let Some( ip) = log.search(path) else {
        return -1;
    };

    let Ok(elf) = ip.read::<ELFHeader>(0) else {
        return -1;
    };

    if !elf.validate_magic() {
        return -1;
    }

    let Ok(mut pagetable) = allocate_pagetable(current_context.trapframe.addr().get()) else {
        return -1;
    };

    macro bad($sz:ident) {{
        free_pagetable(pagetable, $sz);
        return -1;
    }}

    // Load program into memory.
    let mut sz = 0;
    let mut off = elf.phoff;
    for _ in 0..elf.phnum {
        let Ok(ph) = ip.read::<ProgramHeader>(off) else {
            bad!(sz);
        };

        if ph.kind != ProgramHeader::KIND_LOAD {
            continue;
        }

        if ph.memsz < ph.filesz {
            bad!(sz);
        }

        if ph.vaddr + ph.memsz < ph.vaddr {
            bad!(sz);
        }

        match pagetable.grow(sz, ph.vaddr + ph.memsz) {
            Ok(new_size) => sz = new_size,
            Err(_) => bad!(sz),
        }

        if ph.vaddr % PGSIZE != 0 {
            bad!(sz);
        }

        if !load_seg(&mut pagetable, ph.vaddr, &ip, ph.off, ph.filesz) {
            bad!(sz);
        }

        off += core::mem::size_of::<ProgramHeader>();
    }
    drop(ip);
    drop(log);

    let oldsz = current_context.sz;

    // Allocate two pages at the next page boundary.
    // Use the second as the user stack.
    let sz = pg_roundup(sz);
    let Ok(sz1) = pagetable.grow(sz, sz + 2 * PGSIZE) else {
        bad!(sz);
    };
    let sz = sz1;
    pagetable
        .search_entry(sz - 2 * PGSIZE, false)
        .unwrap()
        .set_user_access(false);
    let mut sp = sz;
    let stackbase = sp - PGSIZE;

    // Push argument strings, prepare rest of stack in ustack.
    let mut ustack = [0usize; MAXARG];
    let argv = ptr_to_slice(argv);
    for (i, arg) in argv.iter().map(|arg| CStr::from_ptr(*arg)).enumerate() {
        if i >= MAXARG {
            bad!(sz);
        }

        let len = arg.to_bytes().len() + 1;

        sp -= len;
        sp -= sp % 16; // riscv sp must be 16-byte aligned

        if sp < stackbase {
            bad!(sz);
        }

        if copyout(pagetable, sp, arg.as_ptr() as usize, len) < 0 {
            bad!(sz);
        }

        ustack[i] = sp;
    }
    ustack[argv.len()] = 0;

    // push the array of argv[] pointers.
    sp -= (argv.len() + 1) * core::mem::size_of::<usize>();
    sp -= sp % 16;

    if sp < stackbase {
        bad!(sz);
    }

    if copyout(
        pagetable,
        sp,
        ustack.as_ptr() as usize,
        (argv.len() + 1) * core::mem::size_of::<usize>(),
    ) < 0
    {
        bad!(sz);
    }

    // arguments to user main(argc, argv)
    // argc is returned via the system call return
    // value, which goes in a0.
    current_context.trapframe.as_mut().a1 = sp as u64;

    let old_pagetable = core::mem::replace(&mut current_context.pagetable, pagetable);
    current_context.sz = sz;
    current_context.trapframe.as_mut().epc = elf.entry as u64;
    current_context.trapframe.as_mut().sp = sp as u64;
    free_pagetable(old_pagetable, oldsz);

    // this ends up in a0, the first argument to main(argc, argv)
    argv.len() as i32
}
