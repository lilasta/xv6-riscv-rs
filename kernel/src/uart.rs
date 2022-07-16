//! low-level driver routines for 16550a UART.

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering::*},
};

use crate::{
    lock::{spin::SpinLock, Lock, LockGuard},
    memory_layout::UART0,
    process::cpu,
};

mod reg {
    // the UART control registers.
    // some have different meanings for
    // read vs write.
    // see http://byterunner.com/16550.html
    pub const RHR: usize = 0; // receive holding register (for input bytes)
    pub const THR: usize = 0; // transmit holding register (for output bytes)
    pub const IER: usize = 1; // interrupt enable register
    pub const FCR: usize = 2; // FIFO control register
    pub const ISR: usize = 2; // interrupt status register
    pub const LCR: usize = 3; // line control register
    pub const LSR: usize = 5; // line status register

    pub const IER_RX_ENABLE: u8 = 1 << 0;
    pub const IER_TX_ENABLE: u8 = 1 << 1;

    pub const FCR_FIFO_ENABLE: u8 = 1 << 0;
    pub const FCR_FIFO_CLEAR: u8 = 3 << 1; // clear the content of the two FIFOs

    pub const LCR_EIGHT_BITS: u8 = 3 << 0;
    pub const LCR_BAUD_LATCH: u8 = 1 << 7; // special mode to set baud rate

    pub const LSR_RX_READY: u8 = 1 << 0; // input is waiting to be read from RHR
    pub const LSR_TX_IDLE: u8 = 1 << 5; // THR can accept another character to send
}

struct TransmitBuffer {
    // the transmit output buffer.
    buf: [u8; Self::SIZE],

    // write next to uart_tx_buf[uart_tx_w % UART_TX_BUF_SIZE]
    w: usize,

    // read next from uart_tx_buf[uart_tx_r % UART_TX_BUF_SIZE]
    r: usize,
}

impl TransmitBuffer {
    const SIZE: usize = 32;

    pub const fn new() -> TransmitBuffer {
        Self {
            buf: [0; _],
            w: 0,
            r: 0,
        }
    }

    pub const fn is_full(&self) -> bool {
        self.w == self.r + TransmitBuffer::SIZE
    }

    pub const fn queue(&mut self, value: u8) {
        self.buf[self.w % Self::SIZE] = value;
        self.w += 1;
    }

    pub const fn dequeue(&mut self) -> u8 {
        let value = self.buf[self.r % Self::SIZE];
        self.r += 1;
        value
    }
}

pub struct UART {
    tx: SpinLock<TransmitBuffer>,

    // from printf.c
    panicked: AtomicBool,
}

impl UART {
    pub fn get() -> &'static Self {
        static THIS: UART = UART::new();
        &THIS
    }

    // the UART control registers are memory-mapped
    // at address UART0. this macro returns the
    // address of one of the registers.
    fn reg(reg: usize) -> NonNull<u8> {
        UART0.map_addr(|addr| addr.saturating_add(reg))
    }

    fn read_reg(&self, reg: usize) -> u8 {
        let src = Self::reg(reg).as_ptr();
        unsafe { core::ptr::read_volatile(src) }
    }

    fn write_reg(&self, reg: usize, value: u8) {
        let dst = Self::reg(reg).as_ptr();
        unsafe { core::ptr::write_volatile(dst, value) }
    }

    fn is_panicked(&self) -> bool {
        self.panicked.load(Relaxed)
    }

    fn send<L: Lock<Target = TransmitBuffer>>(&self, mut tx: LockGuard<L>) {
        use reg::*;

        let cpu = cpu::current();

        loop {
            if tx.w == tx.r {
                // transmit buffer is empty.
                return;
            }

            if self.read_reg(LSR) & LSR_TX_IDLE == 0 {
                // the UART transmit holding register is full,
                // so we cannot give it another byte.
                // it will interrupt when it's ready for a new byte.
                return;
            }

            let c = tx.dequeue();

            // maybe uartputc() is waiting for space in the buffer.
            cpu.wakeup(self as *const _ as usize);

            self.write_reg(THR, c);
        }
    }

    pub const fn new() -> Self {
        Self {
            tx: SpinLock::new(TransmitBuffer::new()),
            panicked: AtomicBool::new(false),
        }
    }

    pub fn init(&self) {
        use reg::*;

        // disable interrupts.
        self.write_reg(IER, 0x00);

        // special mode to set baud rate.
        self.write_reg(LCR, LCR_BAUD_LATCH);

        // LSB for baud rate of 38.4K.
        self.write_reg(0, 0x03);

        // MSB for baud rate of 38.4K.
        self.write_reg(1, 0x00);

        // leave set-baud mode,
        // and set word length to 8 bits, no parity.
        self.write_reg(LCR, LCR_EIGHT_BITS);

        // reset and enable FIFOs.
        self.write_reg(FCR, FCR_FIFO_ENABLE | FCR_FIFO_CLEAR);

        // enable transmit and receive interrupts.
        self.write_reg(IER, IER_TX_ENABLE | IER_RX_ENABLE);
    }

    // add a character to the output buffer and tell the
    // UART to start sending if it isn't already.
    // blocks if the output buffer is full.
    // because it may block, it can't be called
    // from interrupts; it's only suitable for use
    // by write().
    pub fn putc(&self, c: u8) {
        let mut tx = self.tx.lock();

        if self.is_panicked() {
            loop {}
        }

        let cpu = cpu::current();

        loop {
            if tx.is_full() {
                // buffer is full.
                // wait for uartstart() to open up space in the buffer.
                cpu.sleep(self as *const _ as usize, &mut tx);
            } else {
                tx.queue(c);
                self.send(tx);
                break;
            }
        }
    }

    // alternate version of uartputc() that doesn't
    // use interrupts, for use by kernel printf() and
    // to echo characters. it spins waiting for the uart's
    // output register to be empty.
    pub fn putc_blocking(&self, c: u8) {
        use reg::*;

        cpu::without_interrupt(|| {
            if self.is_panicked() {
                loop {}
            }

            // wait for Transmit Holding Empty to be set in LSR.
            while self.read_reg(LSR) & LSR_TX_IDLE == 0 {}

            self.write_reg(THR, c);
        })
    }

    pub fn getc(&self) -> Option<u8> {
        use reg::*;

        if self.read_reg(LSR) & 0x01 != 0 {
            Some(self.read_reg(RHR)) // input data is ready.
        } else {
            None
        }
    }

    // handle a uart interrupt, raised because input has
    // arrived, or the uart is ready for more output, or
    // both. called from trap.c.
    pub fn handle_interrupt(&self) {
        // read and process incoming characters.
        loop {
            let c = self.getc();
            match c {
                Some(c) => unsafe { consoleintr(c as i32) },
                None => break,
            }
        }

        // send buffered characters.
        self.send(self.tx.lock());
    }
}

extern "C" {
    fn consoleintr(c: i32);
}

mod binding {
    use super::*;

    #[no_mangle]
    unsafe extern "C" fn uartinit() {
        UART::get().init();
    }

    #[no_mangle]
    unsafe extern "C" fn uartputc(c: i32) {
        UART::get().putc(c as u8);
    }

    #[no_mangle]
    unsafe extern "C" fn uartputc_sync(c: i32) {
        UART::get().putc_blocking(c as u8);
    }

    #[no_mangle]
    unsafe extern "C" fn uartintr() {
        UART::get().handle_interrupt();
    }
}
