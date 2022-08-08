use core::mem::MaybeUninit;

use crate::{
    lock::Lock,
    process,
    vm::binding::{copyin, copyinstr},
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

pub unsafe fn read_from_process_memory<T>(addr: usize) -> Option<T> {
    let process = process::current().unwrap().get_mut().context().unwrap();
    if addr >= process.sz || addr + core::mem::size_of::<T>() > process.sz {
        return None;
    }

    let dst = MaybeUninit::uninit();
    if copyin(
        process.pagetable,
        dst.as_ptr() as usize,
        addr,
        core::mem::size_of::<T>(),
    ) != 0
    {
        return None;
    }

    Some(dst.assume_init())
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

    let process = process::current().unwrap().get_mut().context().unwrap();
    let err = copyinstr(process.pagetable, buf, addr, max);
    if err < 0 {
        Err(err)
    } else {
        Ok(strlen(buf as *const _))
    }
}

unsafe fn arg_raw(n: usize) -> u64 {
    let process = process::current().unwrap().get_mut().context().unwrap();
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

pub unsafe fn arg_int(n: usize) -> i32 {
    arg_raw(n) as _
}

pub unsafe fn arg_addr(n: usize) -> usize {
    arg_raw(n) as _
}

pub unsafe fn arg_string(n: usize, buf: usize, max: usize) -> Result<usize, i32> {
    let addr = arg_addr(n);
    read_string_from_process_memory(addr, buf, max)
}

#[no_mangle]
unsafe extern "C" fn fetchaddr(addr: usize, ip: *mut u64) -> i32 {
    match read_from_process_memory(addr) {
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
    *ip = arg_int(n as _);
    0
}

#[no_mangle]
unsafe extern "C" fn argaddr(n: i32, ip: *mut usize) -> i32 {
    *ip = arg_addr(n as _);
    0
}

#[no_mangle]
unsafe extern "C" fn argstr(n: i32, buf: usize, max: i32) -> i32 {
    match arg_string(n as _, buf, max as _) {
        Ok(v) => v as _,
        Err(v) => v,
    }
}

extern "C" {
    fn sys_chdir() -> u64;
    fn sys_close() -> u64;
    fn sys_dup() -> u64;
    fn sys_exec() -> u64;
    fn sys_exit() -> u64;
    fn sys_fork() -> u64;
    fn sys_fstat() -> u64;
    fn sys_getpid() -> u64;
    fn sys_kill() -> u64;
    fn sys_link() -> u64;
    fn sys_mkdir() -> u64;
    fn sys_mknod() -> u64;
    fn sys_open() -> u64;
    fn sys_pipe() -> u64;
    fn sys_read() -> u64;
    fn sys_sbrk() -> u64;
    fn sys_sleep() -> u64;
    fn sys_unlink() -> u64;
    fn sys_wait() -> u64;
    fn sys_write() -> u64;
    fn sys_uptime() -> u64;
}

static SYSCALLS: &[unsafe extern "C" fn() -> u64] = &[
    sys_fork, sys_exit, sys_wait, sys_pipe, sys_read, sys_kill, sys_exec, sys_fstat, sys_chdir,
    sys_dup, sys_getpid, sys_sbrk, sys_sleep, sys_uptime, sys_open, sys_write, sys_mknod,
    sys_unlink, sys_link, sys_mkdir, sys_close,
];

#[no_mangle]
unsafe extern "C" fn syscall() {
    let process = process::current().unwrap().get_mut().context().unwrap();
    let index = process.trapframe.as_ref().a7 - 1;
    process.trapframe.as_mut().a0 = match SYSCALLS.get(index as usize) {
        Some(f) => f(),
        None => u64::MAX,
    };
}
