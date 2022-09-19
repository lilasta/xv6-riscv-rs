use alloc::{ffi::CString, sync::Arc};
use arrayvec::ArrayVec;

use crate::{
    allocator, clock,
    config::{MAXARG, MAXPATH, NDEV},
    exec::execute,
    file::File,
    fs::{self, InodeGuard, InodeKind},
    log,
    pipe::Pipe,
    process,
    riscv::paging::{PGSIZE, PTE},
};

pub unsafe fn read_string_from_process_memory(addr: usize, buffer: &mut [u8]) -> Result<usize, ()> {
    let process = process::context().unwrap();
    process.pagetable.read_cstr(buffer, addr)
}

fn arg_raw<const N: usize>() -> u64 {
    let process = process::context().unwrap();
    match N {
        0 => process.trapframe.a0,
        1 => process.trapframe.a1,
        2 => process.trapframe.a2,
        3 => process.trapframe.a3,
        4 => process.trapframe.a4,
        5 => process.trapframe.a5,
        _ => panic!(),
    }
}

fn arg_i32<const N: usize>() -> i32 {
    arg_raw::<N>() as _
}

fn arg_usize<const N: usize>() -> usize {
    arg_raw::<N>() as _
}

fn arg_string<const N: usize>(buffer: &mut [u8]) -> Result<&str, ()> {
    let addr = arg_usize::<N>();
    unsafe { read_string_from_process_memory(addr, buffer)? };

    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    core::str::from_utf8(&buffer[..len]).or(Err(()))
}

fn arg_fd<const N: usize>() -> Result<(usize, &'static Arc<File>), ()> {
    let context = process::context().unwrap();
    let fd = arg_usize::<N>();
    let f = context.ofile.get(fd).ok_or(())?;
    let f = f.as_ref().ok_or(())?;
    Ok((fd, f))
}

fn fdalloc(f: Arc<File>) -> Result<usize, ()> {
    let context = process::context().ok_or(())?;
    for (fd, file) in context.ofile.iter_mut().enumerate() {
        if file.is_none() {
            *file = Some(f);
            return Ok(fd);
        }
    }
    Err(())
}

#[inline(always)]
pub unsafe fn syscall(index: usize) -> Result<u64, ()> {
    static LOOKUP: [fn() -> Result<u64, ()>; 21] = [
        sys_fork, sys_exit, sys_wait, sys_pipe, sys_read, sys_kill, sys_exec, sys_fstat, sys_chdir,
        sys_dup, sys_getpid, sys_sbrk, sys_sleep, sys_uptime, sys_open, sys_write, sys_mknod,
        sys_unlink, sys_link, sys_mkdir, sys_close,
    ];

    match index {
        0 => Err(()),
        i @ 1..=21 => LOOKUP[i - 1](),
        _ => Err(()),
    }
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

    let context = process::context().unwrap();
    let size_old = context.sz;
    let size_new = context.sz.wrapping_add_signed(n);

    if n > 0 {
        context.sz = context.pagetable.grow(size_old, size_new, PTE::W)?;
    }
    if n < 0 {
        context.sz = context.pagetable.shrink(size_old, size_new)?;
    }

    Ok(size_old as u64)
}

fn sys_kill() -> Result<u64, ()> {
    Ok(process::kill(arg_usize::<0>()) as u64)
}

fn sys_sleep() -> Result<u64, ()> {
    let time = arg_usize::<0>() as u64;
    if clock::sleep(time) {
        Ok(0)
    } else {
        Err(())
    }
}

fn sys_uptime() -> Result<u64, ()> {
    Ok(clock::get())
}

fn sys_dup() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;
    let fd = fdalloc(f.clone())?;
    Ok(fd as u64)
}

fn sys_read() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;
    let addr = arg_usize::<1>();
    let n = arg_usize::<2>();
    let result = f.read(addr, n);
    result.map(|read| read as u64)
}

fn sys_write() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;

    let addr = arg_usize::<1>();
    let n = arg_usize::<2>();
    let result = f.write(addr, n);
    result.map(|wrote| wrote as u64)
}

fn sys_close() -> Result<u64, ()> {
    let (fd, _) = arg_fd::<0>()?;
    let context = process::context().unwrap();
    context.ofile[fd] = None;
    Ok(0)
}

fn sys_fstat() -> Result<u64, ()> {
    let (_, f) = arg_fd::<0>()?;
    let addr = arg_usize::<1>();

    let context = process::context().unwrap();
    let Ok(stat) = f.stat() else {
        return Err(())
    };

    match unsafe { context.pagetable.write(addr, &stat) } {
        Ok(_) => Ok(0),
        Err(_) => Err(()),
    }
}

fn sys_link() -> Result<u64, ()> {
    let mut old = [0u8; MAXPATH];
    let mut new = [0u8; MAXPATH];

    let old = arg_string::<0>(&mut old)?;
    let new = arg_string::<1>(&mut new)?;

    fs::link(new, old).and(Ok(0))
}

fn sys_unlink() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;
    fs::unlink(path).and(Ok(0))
}

fn sys_open() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;

    const O_RDONLY: usize = 0x000;
    const O_WRONLY: usize = 0x001;
    const O_RDWR: usize = 0x002;
    const O_CREATE: usize = 0x200;
    const O_TRUNC: usize = 0x400;

    let log = log::start();
    let mode = arg_usize::<1>();
    let mut inode = if mode & O_CREATE != 0 {
        fs::create(path, InodeKind::File, 0, 0, &log)?
    } else {
        let inode_ref = fs::search_inode(path, &log).ok_or(())?;
        let inode = inode_ref.lock();
        inode
    };

    if inode.is_directory() && mode != O_RDONLY {
        return Err(());
    }

    if inode.is_device() && inode.device_major() >= Some(NDEV) {
        return Err(());
    }

    let readable = mode & O_WRONLY == 0;
    let writable = mode & O_WRONLY != 0 || mode & O_RDWR != 0;

    let file = if inode.is_device() {
        File::new_device(
            InodeGuard::as_ref(&inode).pin(),
            inode.device_major().unwrap(),
            readable,
            writable,
        )
    } else {
        File::new_inode(InodeGuard::as_ref(&inode).pin(), readable, writable)
    };

    let file = Arc::new(file);
    let Ok(fd) =  fdalloc(file) else {
        return Err(());
    };

    if mode & O_TRUNC != 0 && inode.is_file() {
        inode.truncate(&log);
    }

    Ok(fd as u64)
}

fn sys_mkdir() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;
    fs::make_directory(path).and(Ok(0))
}

fn sys_mknod() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;
    let major = arg_usize::<1>();
    let minor = arg_usize::<2>();
    fs::make_special_file(path, major as u16, minor as u16).and(Ok(0))
}

fn sys_chdir() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;

    let log = log::start();
    let inode_ref = fs::search_inode(path, &log).ok_or(())?;
    let inode = inode_ref.lock();

    if !inode.is_directory() {
        return Err(());
    }

    let context = process::context().unwrap();
    context.cwd.take();
    context
        .cwd
        .replace(inode_ref.pin())
        .map(|cwd| cwd.drop_with_log(&log));
    Ok(0)
}

fn sys_exec() -> Result<u64, ()> {
    let mut path = [0u8; MAXPATH];
    let path = arg_string::<0>(&mut path)?;

    let mut argv = ArrayVec::<_, MAXARG>::new();
    let argv_user = arg_usize::<1>();

    for i in 0.. {
        let addr = process::read_memory(argv_user + core::mem::size_of::<usize>() * i);
        let Some(addr) = addr else {
            return Err(());
        };

        if addr == 0 {
            break;
        }

        let Some(mem) = allocator::get().allocate_page() else {
            return Err(());
        };

        let buffer = unsafe { core::slice::from_raw_parts_mut(mem.as_ptr(), PGSIZE) };
        if unsafe { read_string_from_process_memory(addr, buffer).is_err() } {
            allocator::get().deallocate_page(mem);
            return Err(());
        }

        let arg = unsafe { CString::from_raw(buffer.as_mut_ptr().cast()) };
        if argv.try_push(arg).is_err() {
            return Err(());
        }
    }

    unsafe { execute(path, &argv).map(|argc| argc as u64) }
}

fn sys_pipe() -> Result<u64, ()> {
    let fdarray = arg_usize::<0>();

    let context = process::context().unwrap();

    let Some((read, write)) = Pipe::allocate() else {
        return Err(());
    };

    let rf = Arc::new(File::new_pipe(read));
    let wf = Arc::new(File::new_pipe(write));

    let Ok(fd0) = fdalloc(rf) else {
        return Err(());
    };

    let Ok(fd1) = fdalloc(wf) else {
        context.ofile[fd0] = None;
        return Err(());
    };

    let pair = [fd0 as u32, fd1 as u32];

    if unsafe { context.pagetable.write(fdarray, &pair).is_err() } {
        context.ofile[fd0] = None;
        context.ofile[fd1] = None;
        return Err(());
    }

    Ok(0)
}
