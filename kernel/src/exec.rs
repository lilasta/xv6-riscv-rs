use alloc::ffi::CString;

use crate::{
    config::MAXARG,
    elf::{ELFHeader, ProgramHeader},
    fs::{self, InodeGuard},
    log, process,
    riscv::paging::{pg_roundup, PageTable, PGSIZE, PTE},
};

fn flags2perm(flags: u32) -> u64 {
    let mut perm = 0;
    if flags & 0x1 != 0 {
        perm |= PTE::X;
    }
    if flags & 0x2 != 0 {
        perm |= PTE::W;
    }
    perm
}

fn load_segment(
    pagetable: &mut PageTable,
    va: usize,
    inode: &mut InodeGuard,
    offset: usize,
    size: usize,
) -> bool {
    for i in (0..size).step_by(PGSIZE) {
        let pa = pagetable.virtual_to_physical(va + i).unwrap();
        let n = (size - i).min(PGSIZE);
        if inode.copy_to::<u8>(false, pa, offset + i, n) != Ok(n) {
            return false;
        }
    }
    true
}

pub unsafe fn execute(path: &str, argv: &[CString]) -> Result<usize, ()> {
    let log = log::start();
    let Some(inode_ref) = fs::search_inode(path, &log) else {
        return Err(());
    };
    let mut inode = inode_ref.lock();

    let Ok(elf) = inode.read::<ELFHeader>(0) else {
        return Err(());
    };

    if !elf.validate_magic() {
        return Err(());
    }

    let Some(context) = process::context() else {
        return Err(());
    };

    let Ok(mut pagetable) = process::allocate_pagetable(core::ptr::addr_of!(*context.trapframe).addr()) else {
        return Err(());
    };

    let bad = |mut pagetable, size| {
        process::free_pagetable(&mut pagetable, size);
        Err(())
    };

    // Load program into memory.
    let mut size = 0;
    for offset in (elf.phoff..)
        .step_by(core::mem::size_of::<ProgramHeader>())
        .take(elf.phnum as usize)
    {
        let Ok(header) = inode.read::<ProgramHeader>(offset) else {
            return bad(pagetable, size);
        };

        if header.kind != ProgramHeader::KIND_LOAD {
            continue;
        }

        if header.memsz < header.filesz {
            return bad(pagetable, size);
        }

        if header.vaddr + header.memsz < header.vaddr {
            return bad(pagetable, size);
        }

        if header.vaddr % PGSIZE != 0 {
            return bad(pagetable, size);
        }

        match pagetable.grow(size, header.vaddr + header.memsz, flags2perm(header.flags)) {
            Ok(new_size) => size = new_size,
            Err(_) => return bad(pagetable, size),
        }

        if !load_segment(
            &mut pagetable,
            header.vaddr,
            &mut inode,
            header.off,
            header.filesz,
        ) {
            return bad(pagetable, size);
        }
    }
    drop(inode);
    drop(inode_ref);
    drop(log);

    let old_size = context.sz;

    // Allocate two pages at the next page boundary.
    // Use the second as the user stack.
    let size = pg_roundup(size);
    let Ok(size) = pagetable.grow(size, size + 2 * PGSIZE, PTE::W) else {
        return bad(pagetable, size);
    };

    pagetable
        .search_entry(size - 2 * PGSIZE, false)
        .unwrap()
        .set_user_access(false);

    let mut sp = size;
    let stackbase = sp - PGSIZE;

    // Push argument strings, prepare rest of stack in ustack.
    let mut ustack = [0usize; MAXARG];
    for (i, arg) in argv.iter().enumerate() {
        if i >= MAXARG {
            return bad(pagetable, size);
        }

        let len = arg.to_bytes().len() + 1;

        sp -= len;
        sp -= sp % 16; // riscv sp must be 16-byte aligned

        if sp < stackbase {
            return bad(pagetable, size);
        }

        if pagetable.write(sp, arg.to_bytes_with_nul()).is_err() {
            return bad(pagetable, size);
        }

        ustack[i] = sp;
    }

    // push the array of argv[] pointers.
    sp -= (argv.len() + 1) * core::mem::size_of::<usize>();
    sp -= sp % 16;

    if sp < stackbase {
        return bad(pagetable, size);
    }

    let ustack_with_nul = &ustack[..(argv.len() + 1)];
    if pagetable.write(sp, ustack_with_nul).is_err() {
        return bad(pagetable, size);
    }

    // arguments to user main(argc, argv)
    // argc is returned via the system call return
    // value, which goes in a0.
    context.trapframe.a1 = sp as u64;

    let mut old_pagetable = core::mem::replace(&mut context.pagetable, pagetable);
    context.sz = size;
    context.trapframe.epc = elf.entry as u64;
    context.trapframe.sp = sp as u64;
    process::free_pagetable(&mut old_pagetable, old_size);

    // this ends up in a0, the first argument to main(argc, argv)
    Ok(argv.len())
}
