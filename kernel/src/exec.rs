use core::{
    ffi::{c_char, c_void, CStr},
    mem::MaybeUninit,
};

use crate::{
    config::MAXARG,
    elf::{ELFHeader, ProgramHeader},
    log::LogGuard,
    process::{cpu, process::ProcessContext},
    riscv::paging::{pg_roundup, PageTable, PGSIZE},
    vm::binding::copyout,
};

extern "C" {
    fn namei(path: *const c_char) -> *mut c_void;
    fn readi(ip: *mut c_void, user_dst: i32, dst: usize, off: u32, n: u32) -> i32;
    fn ilock(ip: *mut c_void);
    fn iunlockput(ip: *mut c_void);
}

pub unsafe fn execute(path: *const c_char, argv: *const *const c_char) -> i32 {
    let _logguard = LogGuard::new();

    let ip = namei(path);
    if ip.is_null() {
        return -1;
    }
    ilock(ip);

    let mut elf: MaybeUninit<ELFHeader> = MaybeUninit::uninit();
    // Check ELF header
    let read = readi(
        ip,
        0,
        elf.as_mut_ptr() as usize,
        0,
        core::mem::size_of::<ELFHeader>() as _,
    );
    if read as usize != core::mem::size_of::<ELFHeader>() {
        todo!(); // bad;
    }

    let elf = elf.assume_init();
    if !elf.validate_magic() {
        todo!(); // bad;
    }

    let current_context = cpu::current().process_context().unwrap();
    let Ok(mut pagetable) = ProcessContext::allocate_pagetable(current_context.trapframe.addr().get()) else {
        todo!(); // bad;
    };

    // Load program into memory.
    let mut sz = 0;
    let mut off = elf.phoff;
    for _ in 0..elf.phnum {
        let mut ph: MaybeUninit<ProgramHeader> = MaybeUninit::uninit();
        let read = readi(
            ip,
            0,
            ph.as_mut_ptr() as usize,
            off as _,
            core::mem::size_of::<ProgramHeader>() as _,
        );
        if read as usize != core::mem::size_of::<ProgramHeader>() {
            todo!(); // bad;
        }
        let ph = ph.assume_init();
        if ph.kind != ProgramHeader::KIND_LOAD {
            continue;
        }

        if ph.memsz < ph.filesz {
            todo!(); // bad;
        }

        if ph.vaddr + ph.memsz < ph.vaddr {
            todo!(); // bad;
        }

        let Ok(sz1) = pagetable.grow(sz, ph.vaddr + ph.memsz) else {
            todo!(); // bad;
        };
        sz = sz1;

        if ph.vaddr % PGSIZE != 0 {
            todo!(); // bad;
        }

        if !loadseg(&mut pagetable, ph.vaddr, ip, ph.off, ph.filesz) {
            todo!(); // bad;
        }

        off += core::mem::size_of::<ProgramHeader>();
    }
    iunlockput(ip);
    drop(_logguard);

    let oldsz = current_context.size;

    // Allocate two pages at the next page boundary.
    // Use the second as the user stack.
    let sz = pg_roundup(sz);
    let Ok(sz1) = pagetable.grow(sz, sz + 2 * PGSIZE) else {
        todo!(); // bad;
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
            todo!(); // bad;
        }

        let len = arg.to_bytes().len();

        sp -= len;
        sp -= sp % 16; // riscv sp must be 16-byte aligned

        if sp < stackbase {
            todo!(); // bad;
        }

        if copyout(pagetable, sp, arg.as_ptr() as usize, len) < 0 {
            todo!(); // bad;
        }

        ustack[i] = sp;
    }
    ustack[argv.len()] = 0;

    // push the array of argv[] pointers.
    sp -= (argv.len() + 1) * core::mem::size_of::<usize>();
    sp -= sp % 16;

    if sp < stackbase {
        todo!(); // bad;
    }

    if copyout(
        pagetable,
        sp,
        ustack.as_ptr() as usize,
        (argv.len() + 1) * core::mem::size_of::<usize>(),
    ) < 0
    {
        todo!(); // bad;
    }

    // arguments to user main(argc, argv)
    // argc is returned via the system call return
    // value, which goes in a0.
    current_context.trapframe.as_mut().a1 = sp as u64;

    // Save program name for debugging.
    let _path = CStr::from_ptr(path);
    //let path = path.program_name();
    //char* dummyname = "DUMMY"; // TODO
    //safestrcpy(p->name, last, sizeof(p->name));

    let old_pagetable = core::mem::replace(&mut current_context.pagetable, pagetable);
    current_context.size = sz;
    current_context.trapframe.as_mut().epc = elf.entry as u64;
    current_context.trapframe.as_mut().sp = sp as u64;
    ProcessContext::free_pagetable(old_pagetable, oldsz);

    // this ends up in a0, the first argument to main(argc, argv)
    argv.len() as i32

    /*
    bad:
     if(pagetable)
       proc_freepagetable(pagetable, sz);
     if(ip){
       iunlockput(ip);
       end_op();
     }
     return -1;
       */
}

unsafe fn ptr_to_slice<'a, T>(start: *const T) -> &'a [T] {
    let mut ptr = start;
    while !ptr.is_null() {
        ptr = ptr.offset(1);
    }
    let end = ptr.sub(1);

    core::slice::from_ptr_range(start..end)
}

fn loadseg(
    pagetable: &mut PageTable,
    va: usize,
    ip: *mut c_void,
    offset: usize,
    size: usize,
) -> bool {
    for i in (0..size).step_by(PGSIZE) {
        let pa = pagetable.virtual_to_physical(va + i).unwrap();
        let n = (size - i).min(PGSIZE);
        if unsafe { readi(ip, 0, pa as _, offset as _, n as _) } as usize != n {
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
}
