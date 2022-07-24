#include "types.h"
#include "param.h"
#include "memlayout.h"
#include "riscv.h"
#include "spinlock.h"
#include "proc.h"
#include "defs.h"

struct proc initproc;

extern void forkret(void);
extern void freeproc(struct proc p);
extern struct spinlock* wait_lock();

extern char trampoline[]; // trampoline.S

extern struct proc proc(int);

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

  acquire(wait_lock());

  // Give any children to init.
  reparent(p);

  // Parent might be sleeping in wait().
  wakeup(*p.parent);
  
  extern void exit_glue(int, struct spinlock*);
  exit_glue(status, wait_lock());
}

// Wait for a child process to exit and return its pid.
// Return -1 if this process has no children.
int
wait(uint64 addr)
{
  int havekids, pid;
  struct proc p = myproc();

  acquire(wait_lock());

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
            release(wait_lock());
            return -1;
          }
          freeproc(np);
          release(np.lock);
          release(wait_lock());
          return pid;
        }
        release(np.lock);
      }
    }

    // No point waiting if we don't have any children.
    if(!havekids || *p.killed){
      release(wait_lock());
      return -1;
    }
    
    // Wait for a child to exit.
    sleep(p.original, wait_lock());  //DOC: wait-sleep
  }
}
