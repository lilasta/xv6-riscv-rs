pub mod kernel_stack;
pub mod process;
pub mod trapframe;

use core::{
    cell::OnceCell,
    ffi::{c_char, c_void},
    sync::atomic::AtomicUsize,
};

use arrayvec::ArrayVec;

use crate::{
    config::{NCPU, NPROC},
    context::{swtch, Context},
    cstr,
    lock::{spin::SpinLock, Lock, LockGuard},
    println,
    process::process::ProcessMetadata,
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled, paging::PGSIZE, read_reg},
    vm::binding::{copyin, copyout, uvminit},
};

use self::process::{Process, ProcessContext};

// a user program that calls exec("/init")
// od -t xC initcode
static INITCODE: &[u8] = &[
    0x17, 0x05, 0x00, 0x00, 0x13, 0x05, 0x45, 0x02, 0x97, 0x05, 0x00, 0x00, 0x93, 0x85, 0x35, 0x02,
    0x93, 0x08, 0x70, 0x00, 0x73, 0x00, 0x00, 0x00, 0x93, 0x08, 0x20, 0x00, 0x73, 0x00, 0x00, 0x00,
    0xef, 0xf0, 0x9f, 0xff, 0x2f, 0x69, 0x6e, 0x69, 0x74, 0x00, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

// Switch to scheduler.  Must hold only p->lock
// and have changed proc->state. Saves and restores
// intena because intena is a property of this
// kernel thread, not this CPU. It should
// be proc->intena and proc->noff, but that would
// break in the few places where a lock is held but
// there's no process.
fn switch_to_scheduler(mut proc: LockGuard<SpinLock<Process>>) {
    static mut DUMMY_CONTEXT: Context = Context::zeroed();

    let cpu = CPU::get_current();
    assert!(cpu.disable_interrupt_depth == 1);
    assert!(proc.is_running() == false);
    assert!(unsafe { is_interrupt_enabled() == false });

    let proc_context = proc.context_mut();

    let save_at = match proc_context {
        Some(context) => &mut context.context,
        None => unsafe { &mut DUMMY_CONTEXT },
    };

    unsafe {
        let intena = cpu.is_interrupt_enabled_before;
        swtch(save_at, &cpu.context);
        cpu.is_interrupt_enabled_before = intena;
    }
}

// Copy to either a user address, or kernel address,
// depending on usr_dst.
// Returns 0 on success, -1 on error.
unsafe fn copyout_either(user_dst: bool, dst: usize, src: usize, len: usize) -> bool {
    let proc_context = CPU::get_current().process_context().unwrap();
    if user_dst {
        copyout(proc_context.pagetable, dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

// Copy from either a user address, or kernel address,
// depending on usr_src.
// Returns 0 on success, -1 on error.
unsafe fn copyin_either(dst: usize, user_src: bool, src: usize, len: usize) -> bool {
    let proc_context = CPU::get_current().process_context().unwrap();
    if user_src {
        copyin(proc_context.pagetable, dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

pub enum CPUState<'a> {
    Ready,
    Starting(LockGuard<'a, SpinLock<Process>>, ProcessContext),
    Running(&'a SpinLock<Process>, ProcessContext),
    Yielding,
    Sleeping,
}

impl<'a> CPUState<'a> {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn is_starting(&self) -> bool {
        matches!(self, Self::Starting(_, _))
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_, _))
    }

    pub const fn is_yielding(&self) -> bool {
        matches!(self, Self::Yielding)
    }

    pub const fn is_sleeping(&self) -> bool {
        matches!(self, Self::Sleeping)
    }

    pub fn complete_switch(&mut self) {
        assert!(self.is_starting());
        unsafe {
            let Self::Starting(proc, proc_context) = core::ptr::read(self) else {
                panic!()
            };
            core::ptr::write(
                self,
                Self::Running(SpinLock::ref_from_guard(&proc), proc_context),
            );
        }
    }

    pub fn r#yield(&mut self) -> (&'a SpinLock<Process>, ProcessContext) {
        assert!(self.is_running());
        unsafe {
            let Self::Running(proc, proc_context) = core::ptr::read(self) else {
                panic!()
            };
            core::ptr::write(self, Self::Yielding);
            (proc, proc_context)
        }
    }

    pub fn sleep(&mut self) -> (&'a SpinLock<Process>, ProcessContext) {
        assert!(self.is_running());
        unsafe {
            let Self::Running(proc, proc_context) = core::ptr::read(self) else {
                panic!()
            };
            core::ptr::write(self, Self::Sleeping);
            (proc, proc_context)
        }
    }
}

// Per-CPU state.
pub struct CPU {
    // The process running on this cpu, or null.
    state: CPUState<'static>,

    // swtch() here to enter scheduler().
    context: Context,

    // Depth of push_off() nesting.
    disable_interrupt_depth: u32,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: bool,
}

impl CPU {
    const fn new() -> Self {
        Self {
            state: CPUState::Ready,
            context: Context::zeroed(),
            disable_interrupt_depth: 0,
            is_interrupt_enabled_before: false,
        }
    }

    // TODO: めっちゃ危ない
    pub fn get_current() -> &'static mut Self {
        assert!(unsafe { !is_interrupt_enabled() });

        let cpuid = unsafe { read_reg!(tp) as usize };
        assert!(cpuid < NCPU);

        static mut CPUS: [CPU; NCPU] = [const { CPU::new() }; _];
        unsafe { &mut CPUS[cpuid] }
    }

    pub fn push_disabling_interrupt() {
        // TODO: おそらく順序が大事?
        let is_enabled = unsafe { is_interrupt_enabled() };

        unsafe { disable_interrupt() };

        let cpu = Self::get_current();

        if cpu.disable_interrupt_depth == 0 {
            cpu.is_interrupt_enabled_before = is_enabled;
        }

        cpu.disable_interrupt_depth += 1;
    }

    pub fn pop_disabling_interrupt() {
        assert!(unsafe { !is_interrupt_enabled() },);

        let cpu = CPU::get_current();

        assert!(cpu.disable_interrupt_depth > 0,);

        cpu.disable_interrupt_depth -= 1;

        if cpu.disable_interrupt_depth == 0 {
            if cpu.is_interrupt_enabled_before {
                unsafe { enable_interrupt() }
            }
        }
    }

    pub fn without_interrupt<R>(f: impl FnOnce() -> R) -> R {
        Self::push_disabling_interrupt();
        let ret = f();
        Self::pop_disabling_interrupt();
        ret
    }

    pub fn process(&self) -> Option<&SpinLock<Process>> {
        match self.state {
            CPUState::Running(proc, _) => Some(proc),
            _ => None,
        }
    }

    pub fn process_context(&mut self) -> Option<&mut ProcessContext> {
        match self.state {
            CPUState::Running(_, ref mut proc_context) => Some(proc_context),
            _ => None,
        }
    }

    // Give up the CPU for one scheduling round.
    pub fn pause(&mut self) {
        let (proc, context) = self.state.r#yield();
        let mut proc = proc.lock();
        proc.pause(context).unwrap();
        switch_to_scheduler(proc);
    }

    pub fn run_process(&mut self, mut proc: LockGuard<'static, SpinLock<Process>>) {
        assert!(proc.is_runnable());

        // Switch to chosen process.  It is the process's job
        // to release its lock and then reacquire it
        // before jumping back to us.
        let context = proc.run().unwrap();
        self.state = CPUState::Starting(proc, context);

        // TODO: 汚い
        let CPUState::Starting(_, regs_context) = &self.state else { unreachable!() };
        unsafe { swtch(&mut self.context, &regs_context.context) };

        // Process is done running for now.
        // It should have changed its p->state before coming back.
        self.state = CPUState::Ready;
    }

    pub fn sleep<L: Lock>(&mut self, token: usize, guard: &mut LockGuard<L>) {
        let (proc, context) = self.state.sleep();
        let mut proc = proc.lock();
        Lock::unlock_temporarily(guard, move || {
            proc.sleep(context, token).unwrap();
            switch_to_scheduler(proc);
        })
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

pub struct Parent {
    parent_pid: usize,
    child_pid: usize,
}

pub struct ProcessTable {
    procs: [SpinLock<Process>; NPROC],
    parent_maps: SpinLock<ArrayVec<Parent, NPROC>>,
    next_pid: AtomicUsize,
}

impl ProcessTable {
    pub const fn new() -> Self {
        Self {
            procs: [const { SpinLock::new(Process::Unused) }; _],
            parent_maps: SpinLock::new(ArrayVec::new_const()),
            next_pid: AtomicUsize::new(1),
        }
    }

    fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }

    fn get_unused_proc(&self) -> Option<&SpinLock<Process>> {
        self.procs.iter().find(|proc| proc.lock().is_unused())
    }

    pub fn get() -> &'static Self {
        static mut TABLE: ProcessTable = ProcessTable::new();
        unsafe { &TABLE }
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpinLock<Process>> {
        self.procs.iter()
    }

    // Per-CPU process scheduler.
    // Each CPU calls scheduler() after setting itself up.
    // Scheduler never returns.  It loops, doing:
    //  - choose a process to run.
    //  - swtch to start running that process.
    //  - eventually that process transfers control
    //    via swtch back to the scheduler.
    pub fn scheduler(&'static self) {
        let cpu = CPU::get_current();
        cpu.state = CPUState::Ready;

        loop {
            unsafe { enable_interrupt() };

            for proc in self.iter() {
                let proc = proc.lock();
                if proc.is_runnable() {
                    cpu.run_process(proc);
                }
            }
        }
    }

    // Exit the current process.  Does not return.
    // An exited process remains in the zombie state
    // until its parent calls wait().
    pub fn exit(&self, status: i32) {
        let proc = CPU::get_current().process().unwrap();
        let pid = unsafe { proc.get().metadata().unwrap().pid }; // TODO: Make this safe
        assert!(pid != 1); // not initproc

        let mut parents = self.parent_maps.lock();

        // Give any children to init.
        for entry in parents.iter_mut().filter(|entry| entry.parent_pid == pid) {
            entry.parent_pid = 1; // initproc;
            self.wakeup(1);
        }

        let parent_pid = parents
            .iter()
            .find(|map| map.child_pid == pid)
            .unwrap()
            .parent_pid;

        // Parent might be sleeping in wait().
        self.wakeup(parent_pid);

        let mut proc = proc.lock();
        proc.metadata_mut().unwrap().exit_status = status;
        proc.die().unwrap();

        Lock::unlock(parents);

        // Jump into the scheduler, never to return.
        switch_to_scheduler(proc);

        unreachable!("zombie exit");
    }

    // Wait for a child process to exit and return its pid.
    // Return -1 if this process has no children.
    pub fn wait(&self, addr: usize) -> Option<usize> {
        let proc = CPU::get_current().process().unwrap();
        let context = CPU::get_current().process_context().unwrap();

        let pid = proc.lock().metadata().unwrap().pid;

        let mut parents = self.parent_maps.lock();
        loop {
            // Scan through table looking for exited children.
            let Some(child_at) = parents.iter().position(|entry| entry.parent_pid == pid) else {
                return None;
            };

            let mut child_proc = self.procs[child_at].lock();
            let child_pid = child_proc.metadata().unwrap().pid;
            if child_proc.is_zombie() {
                if addr != 0 {
                    if unsafe {
                        copyout(
                            context.pagetable,
                            addr,
                            &child_proc.metadata().unwrap().exit_status as *const i32 as usize,
                            core::mem::size_of_val(&child_proc.metadata().unwrap().exit_status),
                        )
                    } < 0
                    {
                        return None;
                    }
                }

                child_proc.clear().unwrap();
                parents.remove(child_at);
                Lock::unlock(child_proc);

                return Some(child_pid);
            }

            if unsafe { proc.get().metadata().unwrap().killed } {
                return None;
            }

            CPU::get_current().sleep(pid, &mut parents);
        }
    }

    pub fn wakeup(&self, token: usize) {
        for proc in self.iter() {
            let current_process = match CPU::get_current().process() {
                Some(proc) => proc,
                None => core::ptr::null(),
            };

            if core::ptr::eq(proc, current_process) {
                continue;
            }

            let mut proc = proc.lock();
            if proc.is_sleeping_on(token) {
                proc.wakeup().unwrap();
            }
        }
    }

    pub fn kill(&self, pid: usize) -> bool {
        for proc in self.iter() {
            let mut proc = proc.lock();
            let Some(metadata) = proc.metadata_mut() else {
                continue;
            };

            if metadata.pid == pid {
                metadata.killed = true;

                if proc.is_sleeping() {
                    proc.wakeup().unwrap();
                }

                return true;
            }
        }
        false
    }

    // Create a new process, copying the parent.
    // Sets up child kernel stack to return as if from fork() system call.
    fn fork(&self) -> Option<usize> {
        let cpu = CPU::get_current();
        let proc = cpu.process().unwrap();

        let (pid, name) = {
            let proc = proc.lock();
            let metadata = proc.metadata().unwrap();
            (metadata.pid, metadata.name)
        };

        let new_pid = self.allocate_pid();
        let new_metadata = ProcessMetadata::new(new_pid, name);
        let new_context = cpu.process_context().unwrap().duplicate().ok()?;

        let mut new_proc = self.get_unused_proc().unwrap().lock();
        new_proc.setup(new_metadata, new_context).unwrap();

        self.parent_maps.lock().push(Parent {
            parent_pid: pid,
            child_pid: new_pid,
        });

        Some(new_pid)
    }
}

fn launch_init() {
    extern "C" {
        fn namei(path: *const c_char) -> *mut c_void;
    }

    let table = ProcessTable::get();

    // TODO: name = "initproc"
    let metadata = ProcessMetadata::new(table.allocate_pid(), ['\0'; 16]);

    let context = unsafe {
        let mut context = ProcessContext::allocate().unwrap();

        // allocate one user page and copy init's instructions
        // and data into it.
        uvminit(context.pagetable, INITCODE.as_ptr(), INITCODE.len());
        context.size = PGSIZE;

        // prepare for the very first "return" from kernel to user.
        context.trapframe.as_mut().epc = 0; // user program counter
        context.trapframe.as_mut().sp = PGSIZE as u64; // user stack pointer
        context.cwd = namei(cstr!("/").as_ptr());
        context
    };

    table.get_unused_proc().unwrap().with(|proc| {
        proc.setup(metadata, context).unwrap();
    });
}

mod binding {
    use crate::{lock::spin_c::SpinLockC, riscv::paging::PageTable};

    use super::{trapframe::TrapFrame, *};

    #[no_mangle]
    extern "C" fn push_off() {
        CPU::push_disabling_interrupt();
    }

    #[no_mangle]
    extern "C" fn pop_off() {
        CPU::pop_disabling_interrupt();
    }

    #[no_mangle]
    extern "C" fn is_myproc_killed_glue() -> i32 {
        unsafe {
            CPU::get_current()
                .process()
                .unwrap()
                .get()
                .metadata()
                .unwrap()
                .killed as i32
        }
    }

    #[no_mangle]
    unsafe extern "C" fn either_copyout(user_dst: i32, dst: usize, src: usize, len: usize) -> i32 {
        copyout_either(user_dst != 0, dst, src, len) as i32
    }

    #[no_mangle]
    unsafe extern "C" fn either_copyin(dst: usize, user_src: i32, src: usize, len: usize) -> i32 {
        copyin_either(dst, user_src != 0, src, len) as i32
    }

    // Print a process listing to console.  For debugging.
    // Runs when user types ^P on console.
    // No lock to avoid wedging a stuck machine further.
    #[no_mangle]
    extern "C" fn procdump() {
        println!("");
        for proc in ProcessTable::get().iter() {
            let proc = unsafe { proc.get() };
            if let Some(metadata) = proc.metadata() {
                let state = match proc {
                    Process::Invalid => "invalid",
                    Process::Unused => "unused",
                    Process::Runnable(_, _) => "runnable",
                    Process::Running(_) => "running",
                    Process::Sleeping(_, _, _) => "sleeping",
                    Process::Zombie(_) => "zombie",
                };
                println!("{} {} {:?}", metadata.pid, state, metadata.name);
            }
        }
    }

    #[no_mangle]
    extern "C" fn cpuid() -> i32 {
        unsafe { read_reg!(tp) as i32 }
    }

    #[no_mangle]
    extern "C" fn scheduler() {
        ProcessTable::get().scheduler()
    }

    #[no_mangle]
    extern "C" fn procinit() {}

    #[no_mangle]
    extern "C" fn userinit() {
        launch_init();
    }

    #[no_mangle]
    extern "C" fn sleep(chan: usize, lock: *mut SpinLockC) {
        let mut guard = LockGuard::new(unsafe { &mut *lock });
        CPU::get_current().sleep(chan, &mut guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    extern "C" fn kill(pid: i32) -> i32 {
        ProcessTable::get().kill(pid as usize) as i32
    }

    #[no_mangle]
    extern "C" fn wait(addr: usize) -> i32 {
        match ProcessTable::get().wait(addr) {
            Some(pid) => pid as i32,
            None => -1,
        }
    }

    #[no_mangle]
    extern "C" fn fork() -> i32 {
        match ProcessTable::get().fork() {
            Some(pid) => pid as i32,
            None => -1,
        }
    }

    #[no_mangle]
    extern "C" fn exit(status: i32) {
        ProcessTable::get().exit(status);
    }

    #[no_mangle]
    extern "C" fn r#yield() {
        CPU::get_current().pause();
    }

    #[no_mangle]
    extern "C" fn wakeup(token: usize) {
        ProcessTable::get().wakeup(token)
    }

    #[no_mangle]
    extern "C" fn growproc(n: i32) -> i32 {
        match CPU::get_current()
            .process_context()
            .unwrap()
            .resize_memory(n as isize)
        {
            Ok(_) => 0,
            Err(_) => -1,
        }
    }

    #[no_mangle]
    extern "C" fn glue_pid() -> i32 {
        CPU::get_current()
            .process()
            .unwrap()
            .lock()
            .metadata()
            .unwrap()
            .pid as i32
    }

    #[no_mangle]
    extern "C" fn glue_trapframe() -> *mut TrapFrame {
        CPU::get_current()
            .process_context()
            .unwrap()
            .trapframe
            .as_ptr()
    }

    #[no_mangle]
    extern "C" fn glue_size() -> u64 {
        CPU::get_current().process_context().unwrap().size as _
    }

    #[no_mangle]
    extern "C" fn glue_cwd() -> *mut c_void {
        CPU::get_current().process_context().unwrap().cwd
    }

    #[no_mangle]
    extern "C" fn glue_cwd_write(c: *mut c_void) {
        CPU::get_current().process_context().unwrap().cwd = c;
    }

    #[no_mangle]
    extern "C" fn glue_ofile(index: usize) -> *mut c_void {
        CPU::get_current().process_context().unwrap().ofile[index]
    }

    #[no_mangle]
    extern "C" fn glue_ofile_write(index: usize, p: *mut c_void) {
        CPU::get_current().process_context().unwrap().ofile[index] = p;
    }

    #[no_mangle]
    extern "C" fn glue_is_proc_running() -> i32 {
        CPU::get_current().process().is_some() as i32
    }

    #[no_mangle]
    extern "C" fn glue_kstack() -> u64 {
        CPU::get_current().process_context().unwrap().kstack as u64
    }

    #[no_mangle]
    extern "C" fn glue_killed() -> i32 {
        CPU::get_current()
            .process()
            .unwrap()
            .lock()
            .metadata()
            .unwrap()
            .killed as i32
    }

    #[no_mangle]
    extern "C" fn glue_killed_on() {
        CPU::get_current()
            .process()
            .unwrap()
            .lock()
            .metadata_mut()
            .unwrap()
            .killed = true;
    }

    #[no_mangle]
    extern "C" fn glue_pagetable() -> PageTable {
        CPU::get_current()
            .process_context()
            .unwrap()
            .pagetable
            .clone()
    }

    #[no_mangle]
    extern "C" fn glue_pagetable_write(pt: PageTable) {
        CPU::get_current().process_context().unwrap().pagetable = pt;
    }

    #[no_mangle]
    extern "C" fn proc_pagetable(trapframe: usize) -> u64 {
        match ProcessContext::allocate_pagetable(trapframe) {
            Ok(pt) => pt.as_u64(),
            Err(_) => 0,
        }
    }

    #[no_mangle]
    extern "C" fn proc_freepagetable(pagetable: PageTable, size: usize) {
        ProcessContext::free_pagetable(pagetable, size);
    }
}
