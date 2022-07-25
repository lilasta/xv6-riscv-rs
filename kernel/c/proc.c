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

