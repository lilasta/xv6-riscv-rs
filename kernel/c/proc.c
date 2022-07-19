#include "types.h"
#include "param.h"
#include "memlayout.h"
#include "riscv.h"
#include "spinlock.h"
#include "proc.h"
#include "defs.h"

struct cpu cpus[NCPU];

struct proc initproc;

extern void forkret(void);
extern void freeproc(struct proc p);

extern char trampoline[]; // trampoline.S

// helps ensure that wakeups of wait()ing
// parents are not lost. helps obey the
// memory model when using p->parent.
// must be acquired before any p->lock.
struct spinlock wait_lock;

extern int allocpid(void);

extern struct proc proc(int);

// Allocate a page for each process's kernel stack.
// Map it high in memory, followed by an invalid
// guard page.
void
proc_mapstacks(pagetable_t * kpgtbl) {
  for(int i = 0; i < NPROC; i++) {
    char *pa = kalloc();
    if(pa == 0)
      panic("kalloc");
    uint64 va = KSTACK((int) (i));
    kvmmap(*kpgtbl, va, (uint64)pa, PGSIZE, PTE_R | PTE_W);
  }
}

int is_myproc_killed_glue(void) {
  return *myproc().killed;
}

extern struct proc allocproc(void);

// a user program that calls exec("/init")
// od -t xC initcode
uchar initcode[] = {
  0x17, 0x05, 0x00, 0x00, 0x13, 0x05, 0x45, 0x02,
  0x97, 0x05, 0x00, 0x00, 0x93, 0x85, 0x35, 0x02,
  0x93, 0x08, 0x70, 0x00, 0x73, 0x00, 0x00, 0x00,
  0x93, 0x08, 0x20, 0x00, 0x73, 0x00, 0x00, 0x00,
  0xef, 0xf0, 0x9f, 0xff, 0x2f, 0x69, 0x6e, 0x69,
  0x74, 0x00, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00
};

// Set up first user process.
void
userinit(void)
{
  struct proc p;

  p = allocproc();
  initproc = p;
  
  // allocate one user page and copy init's instructions
  // and data into it.
  uvminit(*p.pagetable, initcode, sizeof(initcode));
  *p.sz = PGSIZE;

  // prepare for the very first "return" from kernel to user.
  (*p.trapframe)->epc = 0;      // user program counter
  (*p.trapframe)->sp = PGSIZE;  // user stack pointer

  safestrcpy(p.name, "initcode", sizeof(16));
  *p.cwd = namei("/");

  *p.state = RUNNABLE;

  release(p.lock);
}

// Create a new process, copying the parent.
// Sets up child kernel stack to return as if from fork() system call.
int
fork(void)
{
  int i, pid;
  struct proc np;
  struct proc p = myproc();

  // Allocate process.
  if((np = allocproc()).original == 0){
    return -1;
  }

  // Copy user memory from parent to child.
  if(uvmcopy(*p.pagetable, *np.pagetable, *p.sz) < 0){
    freeproc(np);
    release(np.lock);
    return -1;
  }
  *np.sz = *p.sz;

  // copy saved user registers.
  *(*np.trapframe) = *(*p.trapframe);

  // Cause fork to return 0 in the child.
  (*np.trapframe)->a0 = 0;

  // increment reference counts on open file descriptors.
  for(i = 0; i < NOFILE; i++)
    if(p.ofile[i])
      np.ofile[i] = filedup(p.ofile[i]);
  *np.cwd = idup(*p.cwd);

  safestrcpy(np.name, p.name, sizeof(16));

  pid = *np.pid;

  release(np.lock);

  acquire(&wait_lock);
  *np.parent = p.original;
  release(&wait_lock);

  acquire(np.lock);
  *np.state = RUNNABLE;
  release(np.lock);

  return pid;
}

// Pass p's abandoned children to init.
// Caller must hold wait_lock.
void
reparent(struct proc p)
{
  for(int i = 0; i < NPROC; i++) {
    struct proc pp = proc(i);
    if(*pp.parent == p.original){
      *pp.parent = initproc.original;
      wakeup(initproc.original);
    }
  }
}

// Exit the current process.  Does not return.
// An exited process remains in the zombie state
// until its parent calls wait().
void
exit(int status)
{
  struct proc p = myproc();

  if(p.original == initproc.original)
    panic("init exiting");

  // Close all open files.
  for(int fd = 0; fd < NOFILE; fd++){
    if(p.ofile[fd]){
      struct file *f = p.ofile[fd];
      fileclose(f);
      p.ofile[fd] = 0;
    }
  }

  begin_op();
  iput(*p.cwd);
  end_op();
  *p.cwd = 0;

  acquire(&wait_lock);

  // Give any children to init.
  reparent(p);

  // Parent might be sleeping in wait().
  wakeup(*p.parent);
  
  acquire(p.lock);

  *p.xstate = status;
  *p.state = ZOMBIE;

  release(&wait_lock);

  // Jump into the scheduler, never to return.
  sched();
  panic("zombie exit");
}

// Wait for a child process to exit and return its pid.
// Return -1 if this process has no children.
int
wait(uint64 addr)
{
  int havekids, pid;
  struct proc p = myproc();

  acquire(&wait_lock);

  for(;;){
    // Scan through table looking for exited children.
    havekids = 0;
    for(int i = 0; i < NPROC; i++) {
      struct proc np = proc(i);
      if(*np.parent == p.original){
        // make sure the child isn't still in exit() or swtch().
        acquire(np.lock);

        havekids = 1;
        if(*np.state == ZOMBIE){
          // Found one.
          pid = *np.pid;
          if(addr != 0 && copyout(*p.pagetable, addr, (char *)np.xstate,
                                  sizeof(*np.xstate)) < 0) {
            release(np.lock);
            release(&wait_lock);
            return -1;
          }
          freeproc(np);
          release(np.lock);
          release(&wait_lock);
          return pid;
        }
        release(np.lock);
      }
    }

    // No point waiting if we don't have any children.
    if(!havekids || *p.killed){
      release(&wait_lock);
      return -1;
    }
    
    // Wait for a child to exit.
    sleep(p.original, &wait_lock);  //DOC: wait-sleep
  }
}

// Per-CPU process scheduler.
// Each CPU calls scheduler() after setting itself up.
// Scheduler never returns.  It loops, doing:
//  - choose a process to run.
//  - swtch to start running that process.
//  - eventually that process transfers control
//    via swtch back to the scheduler.
void
scheduler(void)
{
  struct cpu c = mycpu();
  
  *(c.proc) = 0;
  for(;;){
    // Avoid deadlock by ensuring that devices can interrupt.
    intr_on();

    for(int i = 0; i < NPROC; i++) {
      struct proc p = proc(i);
      acquire(p.lock);
      if(*p.state == RUNNABLE) {
        // Switch to chosen process.  It is the process's job
        // to release its lock and then reacquire it
        // before jumping back to us.
        *p.state = RUNNING;
        *c.proc = p.original;
        swtch(c.context, p.context);

        // Process is done running for now.
        // It should have changed its p->state before coming back.
        *c.proc = 0;
      }
      release(p.lock);
    }
  }
}

// Switch to scheduler.  Must hold only p->lock
// and have changed proc->state. Saves and restores
// intena because intena is a property of this
// kernel thread, not this CPU. It should
// be proc->intena and proc->noff, but that would
// break in the few places where a lock is held but
// there's no process.
void
sched(void)
{
  int intena;
  struct proc p = myproc();

  if(!holding(p.lock))
    panic("sched p->lock");
  if(*mycpu().noff != 1)
    panic("sched locks");
  if(*p.state == RUNNING)
    panic("sched running");
  if(intr_get())
    panic("sched interruptible");

  intena = *mycpu().intena;
  swtch(p.context, mycpu().context);
  *mycpu().intena = intena;
}

// Give up the CPU for one scheduling round.
void
yield(void)
{
  struct proc p = myproc();
  acquire(p.lock);
  *p.state = RUNNABLE;
  sched();
  release(p.lock);
}

// A fork child's very first scheduling by scheduler()
// will swtch to forkret.
void
forkret(void)
{
  static int first = 1;

  // Still holding p->lock from scheduler.
  release(myproc().lock);

  if (first) {
    // File system initialization must be run in the context of a
    // regular process (e.g., because it calls sleep), and thus cannot
    // be run from main().
    first = 0;
    fsinit(ROOTDEV);
  }

  usertrapret();
}

// Print a process listing to console.  For debugging.
// Runs when user types ^P on console.
// No lock to avoid wedging a stuck machine further.
void
procdump(void)
{
  static char *states[] = {
  [UNUSED]    "unused",
  [SLEEPING]  "sleep ",
  [RUNNABLE]  "runble",
  [RUNNING]   "run   ",
  [ZOMBIE]    "zombie"
  };
  char *state;

  printf("\n");
  for(int i = 0; i < NPROC; i++) {
    struct proc p = proc(i);
    if(*p.state == UNUSED)
      continue;
    if(*p.state >= 0 && *p.state < NELEM(states) && states[*p.state])
      state = states[*p.state];
    else
      state = "???";
    printf("%d %s %s", *p.pid, state, p.name);
    printf("\n");
  }
}
