use core::{ffi::CStr, mem::MaybeUninit, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    config::{MAXARG, MAXPATH, NDEV},
    exec::execute,
    file::{
        filealloc, fileclose, filedup, fileread, filestat, filewrite, pipealloc, FileC, FD_DEVICE,
        FD_INODE,
    },
    fs::{self, InodeOps},
    lock::{spin::SpinLock, Lock},
    log, process,
    riscv::paging::PGSIZE,
    vm::binding::{copyinstr, copyout},
};

pub enum SystemCall {
    Fork = 1,
    Exit = 2,
    Wait = 3,
    Pipe = 4,
    Read = 5,
    Kill = 6,
    Exec = 7,
    Fstat = 8,
    Chdir = 9,
    Dup = 10,
    GetPID = 11,
    Sbrk = 12,
    Sleep = 13,
    Uptime = 14,
    Open = 15,
    Write = 16,
    Mknod = 17,
    Unlink = 18,
    Link = 19,
    Mkdir = 20,
    Close = 21,
}

pub unsafe fn read_string_from_process_memory(
    addr: usize,
    buf: usize,
    max: usize,
) -> Result<usize, i32> {
    unsafe fn strlen(mut s: *const u8) -> usize {
        let mut i = 0;
        while *s != 0 {
            s = s.add(1);
            i += 1;
        }
        i
    }

    let process = process::context().unwrap();
    let err = copyinstr(process.pagetable, buf, addr, max);
    if err < 0 {
        Err(err)
    } else {
        Ok(strlen(buf as *const _))
    }
}

fn arg_raw<const N: usize>() -> u64 {
    let process = process::context().unwrap();
    unsafe {
        match N {
            0 => process.trapframe.as_ref().a0,
            1 => process.trapframe.as_ref().a1,
            2 => process.trapframe.as_ref().a2,
            3 => process.trapframe.as_ref().a3,
            4 => process.trapframe.as_ref().a4,
            5 => process.trapframe.as_ref().a5,
            _ => panic!(),
        }
    }
}

fn arg_i32<const N: usize>() -> i32 {
    arg_raw::<N>() as _
}

fn arg_usize<const N: usize>() -> usize {
    arg_raw::<N>() as _
}

fn arg_string<const N: usize>(buf: usize, max: usize) -> Result<usize, i32> {
    let addr = arg_usize::<N>();
    unsafe { read_string_from_process_memory(addr, buf, max) }
}

fn arg_fd<const N: usize>() -> Result<(usize, *mut FileC), ()> {
    let context = process::context().unwrap();
    let fd = arg_usize::<N>();
    let f = context.ofile.get(fd).copied().ok_or(())?;
    if f.is_null() {
        return Err(());
    }
    Ok((fd, f.cast()))
}

fn fdalloc(f: *mut FileC) -> Result<usize, ()> {
    let context = process::context().ok_or(())?;
    for (fd, file) in context.ofile.iter_mut().enumerate() {
        if file.is_null() {
            *file = f.cast();
            return Ok(fd);
        }
    }
    Err(())
}

static SYSCALLS: &[fn() -> Result<u64, ()>] = &[
    sys_fork, sys_exit, sys_wait, sys_pipe, sys_read, sys_kill, sys_exec, sys_fstat, sys_chdir,
    sys_dup, sys_getpid, sys_sbrk, sys_sleep, sys_uptime, sys_open, sys_write, sys_mknod,
    sys_unlink, sys_link, sys_mkdir, sys_close,
];

#[no_mangle]
unsafe extern "C" fn syscall() {
    let process = process::context().unwrap();
    let index = process.trapframe.as_ref().a7 - 1;
    let result = match SYSCALLS.get(index as usize) {
        Some(f) => f().unwrap_or(u64::MAX),
        None => u64::MAX,
    };
    process.trapframe.as_mut().a0 = result;
}

static TICKS: SpinLock<u64> = SpinLock::new(0);

#[no_mangle]
unsafe extern "C" fn clockintr() {
    let mut ticks = TICKS.lock();
    *ticks += 1; // TODO: Overflow?
    process::wakeup(&TICKS as *const _ as usize);
}

fn sys_exit() -> Result<u64, ()> {
    let n = arg_i32::<0>();
    unsafe { process::exit(n) };
    unreachable!()
}

fn sys_getpid() -> Result<u64, ()> {
    Ok(process::id().unwrap() as u64)
}

fn sys_fork() -> Result<u64, ()> {
    unsafe { process::fork().map(|pid| pid as u64).ok_or(()) }
}

fn sys_wait() -> Result<u64, ()> {
    let addr = match arg_usize::<0>() {
        0 => None,
        addr => Some(addr),
    };

    unsafe { process::wait(addr).map(|pid| pid as u64).ok_or(()) }
}

fn sys_sbrk() -> Result<u64, ()> {
    let n = arg_i32::<0>() as isize;
    process::context()
        .unwrap()
        .resize_memory(n)
        .map(|old| old as u64)
}

fn sys_kill() -> Result<u64, ()> {
    Ok(process::kill(arg_usize::<0>()) as u64)
}

fn sys_sleep() -> Result<u64, ()> {
    let time = arg_usize::<0>() as u64;
    let mut ticks = TICKS.lock();
    let ticks0 = *ticks;
    while (*ticks - ticks0) < time {
        if process::is_killed() == Some(true) {
            return Err(());
        }
        process::sleep(&TICKS as *const _ as usize, &mut ticks);
    }
    Ok(0)
}

fn sys_uptime() -> Result<u64, ()> {
    Ok(*TICKS.lock())
}

fn sys_dup() -> Result<u64, ()> {
    let (fd, f) = arg_fd::<0>()?;
    fdalloc(f)?;
    unsafe { filedup(f) };
    Ok(fd as u64)
}

fn sys_read() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;
    let addr = arg_usize::<1>();
    let n = arg_i32::<2>();
    Ok(unsafe { fileread(f, addr, n) as u64 })
}

fn sys_write() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;

    let addr = arg_usize::<1>();
    let n = arg_i32::<2>();
    Ok(unsafe { filewrite(f, addr, n) as u64 })
}

fn sys_close() -> Result<u64, ()> {
    let (fd, f) = arg_fd::<0>()?;
    let context = process::context().unwrap();
    context.ofile[fd] = core::ptr::null_mut();
    unsafe { fileclose(f) };
    Ok(0)
}

fn sys_fstat() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;
    let addr = arg_usize::<1>();
    Ok(unsafe { filestat(f, addr) as u64 })
}

fn sys_link() -> Result<u64, ()> {
    let mut new = [0u8; MAXPATH];
    let mut old = [0u8; MAXPATH];

    arg_string::<0>(old.as_mut_ptr().addr(), old.len()).or(Err(()))?;
    arg_string::<1>(new.as_mut_ptr().addr(), new.len()).or(Err(()))?;

    let old = unsafe { CStr::from_ptr(old.as_ptr().cast()).to_str().or(Err(()))? };
    let new = unsafe { CStr::from_ptr(new.as_ptr().cast()).to_str().or(Err(()))? };

    fs::link(new, old).and(Ok(0))
}

fn sys_unlink() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let path = unsafe { CStr::from_ptr(path.as_ptr().cast()).to_str().or(Err(()))? };

    fs::unlink(path).and(Ok(0))
}

fn sys_open() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let path = unsafe { CStr::from_ptr(path.as_ptr().cast()).to_str().or(Err(()))? };

    const O_RDONLY: usize = 0x000;
    const O_WRONLY: usize = 0x001;
    const O_RDWR: usize = 0x002;
    const O_CREATE: usize = 0x200;
    const O_TRUNC: usize = 0x400;

    let log = log::start();
    let mode = arg_usize::<1>();
    let mut inode = if mode & O_CREATE != 0 {
        log.create(path, 2, 0, 0) // TODO: 2 = T_FILE
    } else {
        log.search(path).ok_or(())
    }?;

    if inode.is_directory() && mode != O_RDONLY {
        return Err(());
    }

    if inode.is_device() && inode.device_major() >= Some(NDEV) {
        return Err(());
    }

    let file = unsafe { filealloc() };
    if file.is_null() {
        return Err(());
    }

    let file = unsafe { &mut *file };
    let Ok(fd) =  fdalloc(file) else {
        unsafe { fileclose(file) };
        return Err(());
    };

    if inode.is_device() {
        file.kind = FD_DEVICE;
        file.major = inode.device_major().unwrap() as u32;
    } else {
        file.kind = FD_INODE;
        file.off = 0;
    }

    file.ip = core::ptr::addr_of_mut!(*inode).cast();
    file.readable = mode & O_WRONLY == 0;
    file.writable = mode & O_WRONLY != 0 || mode & O_RDWR != 0;

    if mode & O_TRUNC != 0 && inode.is_file() {
        inode.truncate();
    }

    inode.unlock_without_put();
    Ok(fd as u64)
}

fn sys_mkdir() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let path = unsafe { CStr::from_ptr(path.as_ptr().cast()).to_str().or(Err(()))? };

    fs::make_directory(path).and(Ok(0))
}

fn sys_mknod() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let path = unsafe { CStr::from_ptr(path.as_ptr().cast()).to_str().or(Err(()))? };

    let major = arg_usize::<1>();
    let minor = arg_usize::<2>();

    fs::make_special_file(path, major as u16, minor as u16).and(Ok(0))
}

fn sys_chdir() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let path = unsafe { CStr::from_ptr(path.as_ptr().cast()).to_str().or(Err(()))? };

    let log = log::start();
    let inode = log.search(path).ok_or(())?;

    if !inode.is_directory() {
        return Err(());
    }

    let context = process::context().unwrap();
    fs::put(context.cwd.cast());
    context.cwd = inode.unlock_without_put().cast();

    Ok(0)
}

fn sys_exec() -> Result<u64, ()> {
    let mut path = [0i8; MAXPATH];

    arg_string::<0>(path.as_mut_ptr().addr(), path.len()).or(Err(()))?;

    let mut argv = [core::ptr::null::<i8>(); MAXARG];
    let argv_user = arg_usize::<1>();

    let deallocate = |argv: [*const i8; _]| {
        for ptr in argv {
            if !ptr.is_null() {
                KernelAllocator::get().deallocate(NonNull::new(ptr.cast_mut()).unwrap());
            }
        }
    };

    let bad = |argv| {
        deallocate(argv);
        Err(())
    };

    let mut i = 0;
    loop {
        if i >= argv.len() {
            return bad(argv);
        }

        let addr = unsafe { process::read_memory(argv_user + core::mem::size_of::<usize>() * i) };
        let Some(addr) = addr else {
            return bad(argv);
        };

        if addr == 0 {
            break;
        }

        let arg = KernelAllocator::get().allocate::<i8>();
        match arg {
            Some(arg) => argv[i] = arg.as_ptr().cast_const(),
            None => return bad(argv),
        }

        if unsafe { read_string_from_process_memory(addr, argv[i].addr(), PGSIZE).is_err() } {
            return bad(argv);
        }

        i += 1;
    }

    let ret = unsafe { execute(process::context().unwrap(), path.as_ptr(), argv.as_ptr()) };
    deallocate(argv);
    Ok(ret as u64)
}

fn sys_pipe() -> Result<u64, ()> {
    let fdarray = arg_usize::<0>();

    let context = process::context().unwrap();
    let mut rf = MaybeUninit::uninit();
    let mut wf = MaybeUninit::uninit();

    if unsafe { pipealloc(rf.as_mut_ptr(), wf.as_mut_ptr()) < 0 } {
        return Err(());
    }

    let rf = unsafe { rf.assume_init() };
    let wf = unsafe { wf.assume_init() };

    let Ok(fd0) = fdalloc(rf) else {
        unsafe {
            fileclose(rf);
            fileclose(wf);
        }
        return Err(());
    };

    let Ok(fd1) = fdalloc(wf) else {
        context.ofile[fd0] = core::ptr::null_mut();
        unsafe {
            fileclose(rf);
            fileclose(wf);
        }
        return Err(());
    };

    if unsafe {
        copyout(
            context.pagetable,
            fdarray,
            core::ptr::addr_of!(fd0).addr(),
            core::mem::size_of::<u32>(),
        ) < 0
            || copyout(
                context.pagetable,
                fdarray + core::mem::size_of::<u32>(),
                core::ptr::addr_of!(fd1).addr(),
                core::mem::size_of::<u32>(),
            ) < 0
    } {
        context.ofile[fd0] = core::ptr::null_mut();
        context.ofile[fd1] = core::ptr::null_mut();
        unsafe {
            fileclose(rf);
            fileclose(wf);
        }
        return Err(());
    }

    Ok(0)
}
