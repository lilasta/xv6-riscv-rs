use core::ffi::c_void;

use crate::{
    config::NOFILE,
    lock::{spin::SpinLock, Lock},
    process,
    vm::binding::copyinstr,
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

#[no_mangle]
unsafe extern "C" fn fetchaddr(addr: usize, ip: *mut u64) -> i32 {
    match process::read_memory(addr) {
        Some(v) => {
            *ip = v;
            0
        }
        None => -1,
    }
}

#[no_mangle]
unsafe extern "C" fn fetchstr(addr: usize, buf: usize, max: i32) -> i32 {
    match read_string_from_process_memory(addr, buf, max as usize) {
        Ok(len) => len as _,
        Err(err) => err as _,
    }
}

#[no_mangle]
unsafe extern "C" fn argint(n: i32, ip: *mut i32) -> i32 {
    *ip = arg_i32(n as _);
    0
}

unsafe fn argfd(n: i32) -> Option<(usize, *mut c_void)> {
    let context = process::context().unwrap();
    let fd = arg_usize(n as _);
    let f = context.ofile.get(fd).copied()?;
    if f.is_null() {
        return None;
    }
    Some((fd, f))
}

#[no_mangle]
unsafe extern "C" fn argaddr(n: i32, ip: *mut usize) -> i32 {
    *ip = arg_usize(n as _);
    0
}

#[no_mangle]
unsafe extern "C" fn argstr(n: i32, buf: usize, max: i32) -> i32 {
    match arg_string(n as _, buf, max as _) {
        Ok(v) => v as _,
        Err(v) => v,
    }
}

fn fdalloc(f: *mut c_void) -> Result<usize, ()> {
    let context = process::context().ok_or(())?;
    for (fd, file) in context.ofile.iter_mut().enumerate() {
        if file.is_null() {
            *file = f;
            return Ok(fd);
        }
    }
    Err(())
}

extern "C" {
    fn sys_chdir() -> u64;
    fn sys_exec() -> u64;
    fn sys_link() -> u64;
    fn sys_mkdir() -> u64;
    fn sys_mknod() -> u64;
    fn sys_open() -> u64;
    fn sys_pipe() -> u64;
    fn sys_unlink() -> u64;
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

extern "C" {
    fn filedup(f: *mut c_void) -> *mut c_void;
    fn fileread(f: *mut c_void, addr: usize, size: i32) -> i32;
    fn filewrite(f: *mut c_void, addr: usize, size: i32) -> i32;
    fn fileclose(f: *mut c_void);
    fn filestat(f: *mut c_void, addr: usize) -> i32;
}

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
