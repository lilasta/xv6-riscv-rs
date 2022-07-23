use core::{
    ffi::{c_char, c_void, CStr},
    mem::MaybeUninit,
};

use crate::{
    config::MAXARG,
    elf::{ELFHeader, ProgramHeader},
    fs::InodeLockGuard,
    lock::Lock,
    log::LogGuard,
    process::{allocate_pagetable, cpu, free_pagetable},
    riscv::paging::{pg_roundup, PageTable, PGSIZE},
    vm::binding::copyout,
};

extern "C" {
    fn namei(path: *const c_char) -> *mut c_void;
    fn readi(ip: *mut c_void, user_dst: i32, dst: usize, off: u32, n: u32) -> i32;
}

pub unsafe fn execute(path: *const c_char, argv: *const *const c_char) -> i32 {
    let _logguard = LogGuard::new();

    let ip = namei(path);
    if ip.is_null() {
        return -1;
    }
    let ip = InodeLockGuard::new(ip);

    let mut elf: MaybeUninit<ELFHeader> = MaybeUninit::uninit();
    // Check ELF header
    let read = readi(
        *ip,
        0,
        elf.as_mut_ptr() as usize,
        0,
        core::mem::size_of::<ELFHeader>() as _,
    );
    if read as usize != core::mem::size_of::<ELFHeader>() {
        return -1;
    }

    let elf = elf.assume_init();
    if !elf.validate_magic() {
        return -1;
    }

    let current_context = cpu::process().unwrap().get_mut();
    let Ok(mut pagetable) = allocate_pagetable(current_context.trapframe.addr()) else {
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
        let mut ph: MaybeUninit<ProgramHeader> = MaybeUninit::uninit();
        let read = readi(
            *ip,
            0,
            ph.as_mut_ptr() as usize,
            off as _,
            core::mem::size_of::<ProgramHeader>() as _,
        );
        if read as usize != core::mem::size_of::<ProgramHeader>() {
            bad!(sz);
        }
        let ph = ph.assume_init();
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

        if !load_seg(&mut pagetable, ph.vaddr, *ip, ph.off, ph.filesz) {
            bad!(sz);
        }

        off += core::mem::size_of::<ProgramHeader>();
    }
    drop(ip);
    drop(_logguard);

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
    (*current_context.trapframe).a1 = sp as u64;

    // Save program name for debugging.
    let _path = CStr::from_ptr(path);
    //let path = path.program_name();
    //char* dummyname = "DUMMY"; // TODO
    //safestrcpy(p->name, last, sizeof(p->name));

    let old_pagetable = core::mem::replace(current_context.pagetable.as_mut().unwrap(), pagetable);
    current_context.sz = sz;
    (*current_context.trapframe).epc = elf.entry as u64;
    (*current_context.trapframe).sp = sp as u64;
    free_pagetable(old_pagetable, oldsz);

    // this ends up in a0, the first argument to main(argc, argv)
    argv.len() as i32
}

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
    ip: *mut c_void,
    offset: usize,
    size: usize,
) -> bool {
    for i in (0..size).step_by(PGSIZE) {
        let pa = pagetable.virtual_to_physical(va + i).unwrap();
        let n = (size - i).min(PGSIZE);
        if unsafe { readi(ip, 0, pa, (offset + i) as _, n as _) } as usize != n {
            return false;
        }
    }
    true
}

mod binding {
    use super::*;

    #[no_mangle]
    unsafe extern "C" fn exec(path: *const c_char, argv: *const *const c_char) -> i32 {
        execute(path, argv)
    }

    #[no_mangle]
    unsafe extern "C" fn loadseg(
        mut pagetable: PageTable,
        va: usize,
        ip: *mut c_void,
        offset: u32,
        size: u32,
    ) -> i32 {
        match load_seg(&mut pagetable, va, ip, offset as _, size as _) {
            true => 0,
            false => -1,
        }
    }
}
