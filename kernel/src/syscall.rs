use core::{
    ffi::{c_void, CStr},
    mem::MaybeUninit,
    ptr::NonNull,
};

use crate::{
    allocator::KernelAllocator,
    config::{MAXARG, MAXPATH, NDEV},
    exec::execute,
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

unsafe fn arg_raw(n: usize) -> u64 {
    let process = process::context().unwrap();
    match n {
        0 => process.trapframe.as_ref().a0,
        1 => process.trapframe.as_ref().a1,
        2 => process.trapframe.as_ref().a2,
        3 => process.trapframe.as_ref().a3,
        4 => process.trapframe.as_ref().a4,
        5 => process.trapframe.as_ref().a5,
        _ => panic!(),
    }
}

pub unsafe fn arg_i32(n: usize) -> i32 {
    arg_raw(n) as _
}

pub unsafe fn arg_usize(n: usize) -> usize {
    arg_raw(n) as _
}

pub unsafe fn arg_string(n: usize, buf: usize, max: usize) -> Result<usize, i32> {
    let addr = arg_usize(n);
    read_string_from_process_memory(addr, buf, max)
}

unsafe fn argfd(n: i32) -> Option<(usize, *mut FileC)> {
    let context = process::context().unwrap();
    let fd = arg_usize(n as _);
    let f = context.ofile.get(fd).copied()?;
    if f.is_null() {
        return None;
    }
    Some((fd, f.cast()))
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

static SYSCALLS: &[unsafe extern "C" fn() -> u64] = &[
    sys_fork, sys_exit, sys_wait, sys_pipe, sys_read, sys_kill, sys_exec, sys_fstat, sys_chdir,
    sys_dup, sys_getpid, sys_sbrk, sys_sleep, sys_uptime, sys_open, sys_write, sys_mknod,
    sys_unlink, sys_link, sys_mkdir, sys_close,
];

#[no_mangle]
unsafe extern "C" fn syscall() {
    let process = process::context().unwrap();
    let index = process.trapframe.as_ref().a7 - 1;
    process.trapframe.as_mut().a0 = match SYSCALLS.get(index as usize) {
        Some(f) => f(),
        None => u64::MAX,
    };
}

static TICKS: SpinLock<u64> = SpinLock::new(0);

#[no_mangle]
unsafe extern "C" fn clockintr() {
    let mut ticks = TICKS.lock();
    *ticks += 1; // TODO: Overflow?
    process::wakeup(&TICKS as *const _ as usize);
}

unsafe extern "C" fn sys_exit() -> u64 {
    let n = arg_i32(0);
    process::exit(n);
    unreachable!()
}

unsafe extern "C" fn sys_getpid() -> u64 {
    process::id().unwrap() as u64
}

unsafe extern "C" fn sys_fork() -> u64 {
    match process::fork() {
        Some(pid) => pid as u64,
        None => u64::MAX,
    }
}

unsafe extern "C" fn sys_wait() -> u64 {
    let addr = match arg_usize(0) {
        0 => None,
        addr => Some(addr),
    };

    match process::wait(addr) {
        Some(pid) => pid as u64,
        None => u64::MAX,
    }
}

unsafe extern "C" fn sys_sbrk() -> u64 {
    let n = arg_i32(0) as isize;
    match process::context().unwrap().resize_memory(n) {
        Ok(old) => old as u64,
        Err(_) => u64::MAX,
    }
}

unsafe extern "C" fn sys_kill() -> u64 {
    process::kill(arg_usize(0)) as u64
}

unsafe extern "C" fn sys_sleep() -> u64 {
    let time = arg_usize(0) as u64;
    let mut ticks = TICKS.lock();
    let ticks0 = *ticks;
    while (*ticks - ticks0) < time {
        if process::is_killed() == Some(true) {
            return u64::MAX;
        }
        process::sleep(&TICKS as *const _ as usize, &mut ticks);
    }
    0
}

unsafe extern "C" fn sys_uptime() -> u64 {
    *TICKS.lock()
}

#[repr(C)]
struct FileC {
    kind: u32,
    refcnt: u32,
    readable: bool,
    writable: bool,
    pipe: *mut c_void,
    ip: *mut c_void,
    off: u32,
    major: u32,
}

extern "C" {
    fn filedup(f: *mut FileC) -> *mut FileC;
    fn fileread(f: *mut FileC, addr: usize, size: i32) -> i32;
    fn filewrite(f: *mut FileC, addr: usize, size: i32) -> i32;
    fn fileclose(f: *mut FileC);
    fn filestat(f: *mut FileC, addr: usize) -> i32;
    fn filealloc() -> *mut FileC;
    fn pipealloc(f1: *mut *mut FileC, f2: *mut *mut FileC) -> i32;
}

const FD_NONE: u32 = 0;
const FD_PIPE: u32 = 1;
const FD_INODE: u32 = 2;
const FD_DEVICE: u32 = 3;

unsafe extern "C" fn sys_dup() -> u64 {
    let Some((fd, f)) = argfd(0) else {
        return u64::MAX;
    };

    let Ok(_) = fdalloc(f) else {
        return u64::MAX;
    };

    filedup(f);
    fd as u64
}

unsafe extern "C" fn sys_read() -> u64 {
    let Some((_, f)) = argfd(0) else {
        return u64::MAX;
    };

    let addr = arg_usize(1);
    let n = arg_i32(2);
    fileread(f, addr, n) as u64
}

unsafe extern "C" fn sys_write() -> u64 {
    let Some((_, f)) = argfd(0) else {
        return u64::MAX;
    };

    let addr = arg_usize(1);
    let n = arg_i32(2);
    filewrite(f, addr, n) as u64
}

unsafe extern "C" fn sys_close() -> u64 {
    let Some((fd, f)) = argfd(0) else {
        return u64::MAX;
    };

    let context = process::context().unwrap();
    context.ofile[fd] = core::ptr::null_mut();
    fileclose(f);
    0
}

unsafe extern "C" fn sys_fstat() -> u64 {
    let Some((_, f)) = argfd(0) else {
        return u64::MAX;
    };

    let addr = arg_usize(1);
    filestat(f, addr) as u64
}

unsafe extern "C" fn sys_link() -> u64 {
    let mut new = [0u8; MAXPATH];
    let mut old = [0u8; MAXPATH];

    if arg_string(0, old.as_mut_ptr().addr(), old.len()).is_err() {
        return u64::MAX;
    }

    if arg_string(1, new.as_mut_ptr().addr(), new.len()).is_err() {
        return u64::MAX;
    }

    let Ok(old) = CStr::from_ptr(old.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    let Ok(new) = CStr::from_ptr(new.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    match fs::link(new, old) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

unsafe extern "C" fn sys_unlink() -> u64 {
    let mut path = [0u8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let Ok(path) = CStr::from_ptr(path.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    match fs::unlink(path) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

unsafe extern "C" fn sys_open() -> u64 {
    let mut path = [0u8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let Ok(path) = CStr::from_ptr(path.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    const O_RDONLY: usize = 0x000;
    const O_WRONLY: usize = 0x001;
    const O_RDWR: usize = 0x002;
    const O_CREATE: usize = 0x200;
    const O_TRUNC: usize = 0x400;

    let log = log::start();
    let mode = arg_usize(1);
    let inode = if mode & O_CREATE != 0 {
        log.create(path, 2, 0, 0) // TODO: 2 = T_FILE
    } else {
        log.search(path).ok_or(())
    };

    let Ok(mut inode) = inode else {
        return u64::MAX;
    };

    if inode.is_directory() && mode != O_RDONLY {
        return u64::MAX;
    }

    if inode.is_device() && inode.device_major() >= Some(NDEV) {
        return u64::MAX;
    }

    let file = unsafe { filealloc() };
    if file.is_null() {
        return u64::MAX;
    }

    let file = unsafe { &mut *file };
    let Ok(fd) =  fdalloc(file) else {
        unsafe { fileclose(file) };
        return u64::MAX;
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
    fd as u64
}

unsafe extern "C" fn sys_mkdir() -> u64 {
    let mut path = [0u8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let Ok(path) = CStr::from_ptr(path.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    match fs::make_directory(path) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

unsafe extern "C" fn sys_mknod() -> u64 {
    let mut path = [0u8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let Ok(path) = CStr::from_ptr(path.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    let major = arg_usize(1);
    let minor = arg_usize(2);

    match fs::make_special_file(path, major as u16, minor as u16) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

unsafe extern "C" fn sys_chdir() -> u64 {
    let mut path = [0u8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let Ok(path) = CStr::from_ptr(path.as_ptr().cast()).to_str() else {
        return u64::MAX;
    };

    let log = log::start();
    let Some(inode) = log.search(path) else {
        return u64::MAX;
    };

    if inode.is_directory() {
        return u64::MAX;
    }

    let context = process::context().unwrap();
    fs::put(context.cwd.cast());
    context.cwd = inode.unlock_without_put().cast();

    0
}

unsafe extern "C" fn sys_exec() -> u64 {
    let mut path = [0i8; MAXPATH];

    if arg_string(0, path.as_mut_ptr().addr(), path.len()).is_err() {
        return u64::MAX;
    }

    let mut argv = [core::ptr::null::<i8>(); MAXARG];
    let argv_user = arg_usize(1);

    let deallocate = |argv: [*const i8; _]| {
        for ptr in argv {
            if !ptr.is_null() {
                KernelAllocator::get().deallocate(NonNull::new_unchecked(ptr.cast_mut()));
            }
        }
    };

    let bad = |argv| {
        deallocate(argv);
        u64::MAX
    };

    let mut i = 0;
    loop {
        if i >= argv.len() {
            return bad(argv);
        }

        let Some(addr) = process::read_memory(argv_user + core::mem::size_of::<usize>() * i) else {
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

        if read_string_from_process_memory(addr, argv[i].addr(), PGSIZE).is_err() {
            return bad(argv);
        }

        i += 1;
    }

    let ret = execute(process::context().unwrap(), path.as_ptr(), argv.as_ptr());
    deallocate(argv);
    ret as u64
}

unsafe extern "C" fn sys_pipe() -> u64 {
    let fdarray = arg_usize(0);

    let context = process::context().unwrap();
    let mut rf = MaybeUninit::uninit();
    let mut wf = MaybeUninit::uninit();

    if pipealloc(rf.as_mut_ptr(), wf.as_mut_ptr()) < 0 {
        return u64::MAX;
    }

    let rf = rf.assume_init();
    let wf = wf.assume_init();

    let Ok(fd0) = fdalloc(rf) else {
        fileclose(rf);
        fileclose(wf);
        return u64::MAX;
    };

    let Ok(fd1) = fdalloc(wf) else {
        context.ofile[fd0] = core::ptr::null_mut();
        fileclose(rf);
        fileclose(wf);
        return u64::MAX;
    };

    if copyout(
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
    {
        context.ofile[fd0] = core::ptr::null_mut();
        context.ofile[fd1] = core::ptr::null_mut();
        fileclose(rf);
        fileclose(wf);
        return u64::MAX;
    }

    0
}
