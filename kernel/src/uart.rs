//! low-level driver routines for 16550a UART.

use core::sync::atomic::{AtomicBool, Ordering::*};

use crate::{
    console::consoleintr,
    interrupt, process,
    spinlock::{SpinLock, SpinLockGuard},
};

mod reg {
    use core::ptr::NonNull;

    use crate::memory_layout::UART0;

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

    // the UART control registers are memory-mapped
    // at address UART0. this macro returns the
    // address of one of the registers.
    fn ptr(reg: usize) -> NonNull<u8> {
        UART0.map_addr(|addr| addr.saturating_add(reg))
    }

    pub unsafe fn read(reg: usize) -> u8 {
        ptr(reg).as_ptr().read_volatile()
    }

    pub unsafe fn write(reg: usize, value: u8) {
        ptr(reg).as_ptr().write_volatile(value)
    }
}

struct TransmitBuffer<const SIZE: usize> {
    // the transmit output buffer.
    buffer: [u8; SIZE],

    // write next to uart_tx_buf[uart_tx_w % UART_TX_BUF_SIZE]
    write_at: usize,

    // read next from uart_tx_buf[uart_tx_r % UART_TX_BUF_SIZE]
    read_at: usize,
}

impl<const SIZE: usize> TransmitBuffer<SIZE> {
    pub const fn new() -> Self {
        Self {
            buffer: [0; _],
            write_at: 0,
            read_at: 0,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.write_at == self.read_at
    }

    pub const fn is_full(&self) -> bool {
        self.write_at == self.read_at + SIZE
    }

    pub const fn queue(&mut self, value: u8) {
        self.buffer[self.write_at % SIZE] = value;
        self.write_at += 1; // TODO: Overflow?
    }

    pub const fn dequeue(&mut self) -> u8 {
        let value = self.buffer[self.read_at % SIZE];
        self.read_at += 1; // TODO: Overflow?
        value
    }
}

pub struct UART {
    tx: SpinLock<TransmitBuffer<32>>,
    panicked: AtomicBool,
}

impl UART {
    pub const fn new() -> Self {
        Self {
            tx: SpinLock::new(TransmitBuffer::new()),
            panicked: AtomicBool::new(false),
        }
    }

    fn is_panicked(&self) -> bool {
        self.panicked.load(Relaxed)
    }

    unsafe fn send(mut tx: SpinLockGuard<'static, TransmitBuffer<32>>) {
        use reg::*;

        while !tx.is_empty() {
            if reg::read(LSR) & LSR_TX_IDLE == 0 {
                // the UART transmit holding register is full,
                // so we cannot give it another byte.
                // it will interrupt when it's ready for a new byte.
                break;
            }

            let ch = tx.dequeue();

            // maybe uartputc() is waiting for space in the buffer.
            process::wakeup(&*tx as *const _ as usize);

            reg::write(THR, ch);
        }
    }

    pub unsafe fn init(&self) {
        use reg::*;

        // disable interrupts.
        reg::write(IER, 0x00);

        // special mode to set baud rate.
        reg::write(LCR, LCR_BAUD_LATCH);

        // LSB for baud rate of 38.4K.
        reg::write(0, 0x03);

        // MSB for baud rate of 38.4K.
        reg::write(1, 0x00);

        // leave set-baud mode,
        // and set word length to 8 bits, no parity.
        reg::write(LCR, LCR_EIGHT_BITS);

        // reset and enable FIFOs.
        reg::write(FCR, FCR_FIFO_ENABLE | FCR_FIFO_CLEAR);

        // enable transmit and receive interrupts.
        reg::write(IER, IER_TX_ENABLE | IER_RX_ENABLE);
    }

    // add a character to the output buffer and tell the
    // UART to start sending if it isn't already.
    // blocks if the output buffer is full.
    // because it may block, it can't be called
    // from interrupts; it's only suitable for use
    // by write().
    pub fn putc(&'static self, c: u8) {
        let mut tx = self.tx.lock();

        if self.is_panicked() {
            loop {
                core::arch::riscv64::pause();
                core::hint::spin_loop();
            }
        }

        loop {
            if tx.is_full() {
                // buffer is full.
                // wait for uartstart() to open up space in the buffer.
                process::sleep(&*tx as *const _ as usize, &mut tx);
            } else {
                tx.queue(c);
                unsafe { Self::send(tx) };
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

        interrupt::off(|| {
            if self.is_panicked() {
                loop {
                    core::arch::riscv64::pause();
                    core::hint::spin_loop();
                }
            }

            // wait for Transmit Holding Empty to be set in LSR.
            while unsafe { reg::read(LSR) & LSR_TX_IDLE == 0 } {}

            unsafe { reg::write(THR, c) };
        })
    }

    pub fn getc(&self) -> Option<u8> {
        use reg::*;

        unsafe {
            if reg::read(LSR) & 0x01 != 0 {
                Some(reg::read(RHR)) // input data is ready.
            } else {
                None
            }
        }
    }

    // handle a uart interrupt, raised because input has
    // arrived, or the uart is ready for more output, or
    // both. called from trap.c.
    pub fn handle_interrupt(&'static self) {
        // read and process incoming characters.
        loop {
            let c = self.getc();
            match c {
                Some(c) => consoleintr(c as i32),
                None => break,
            }
        }

        // send buffered characters.
        unsafe { Self::send(self.tx.lock()) };
    }
}

pub fn get() -> &'static UART {
    static UART: UART = UART::new();
    &UART
}

pub unsafe fn uartintr() {
    get().handle_interrupt();
}
