use crate::{
    config::NCPU,
    context::{switch, Context},
    interrupt, riscv,
};

static mut CONTEXTS: [Context; NCPU] = [const { Context::zeroed() }; _];

pub fn id() -> usize {
    assert!(!interrupt::is_enabled());
    unsafe { riscv::read_reg!(tp) as usize }
}

pub unsafe fn dispatch(context: &Context) {
    switch(&mut CONTEXTS[id()], context);
}

pub unsafe fn preemption(context: &mut Context) {
    switch(context, &CONTEXTS[id()]);
}
