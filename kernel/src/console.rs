//!
//! Console input and output, to the uart.
//! Reads are line at a time.
//! Implements special input characters:
//!   newline -- end of line
//!   control-h -- backspace
//!   control-u -- kill line
//!   control-d -- end of file
//!   control-p -- print process list
//!

use crate::{
    file::{DeviceFile, DEVICEFILES},
    process,
    spinlock::{SpinLock, SpinLockGuard},
    uart::UART,
};

const fn ctrl(x: char) -> u8 {
    (x as u8).wrapping_sub(b'@')
}

pub struct Console {
    buf: [u8; Self::INPUT_BUF_LEN],
    read_index: usize,
    write_index: usize,
    edit_index: usize,
}

impl Console {
    const INPUT_BUF_LEN: usize = 128;

    pub const fn new() -> Self {
        Self {
            buf: [0; _],
            read_index: 0,
            write_index: 0,
            edit_index: 0,
        }
    }

    pub fn backspace() {
        let uart = UART::get();
        uart.putc_blocking(0x08); // '\b'
        uart.putc_blocking(0x20); // ' '
        uart.putc_blocking(0x08); // '\b'
    }

    //
    // send one character to the uart.
    // called by printf, and to echo input characters,
    // but not from write().
    //
    pub fn putc(c: u8) {
        UART::get().putc_blocking(c);
    }

    //
    // user write()s to the console go here.
    //
    pub fn write(src: usize, n: usize) -> usize {
        for i in 0..n {
            match process::read_memory(src + i) {
                Some(ch) => Self::putc(ch),
                None => return i,
            }
        }
        n
    }

    //
    // the console input interrupt handler.
    // uartintr() calls this for input character.
    // do erase/kill processing, append to cons.buf,
    // wake up consoleread() if a whole line has arrived.
    //
    pub fn handle_interrupt(&mut self, c: u8) {
        match c {
            const { ctrl('P') } => {
                process::procdump();
            }
            const { ctrl('U') } => {
                while self.edit_index != self.write_index
                    && self.buf[(self.edit_index - 1) % Self::INPUT_BUF_LEN] != b'\n'
                {
                    self.edit_index -= 1;
                    Self::backspace();
                }
            }
            c if c == 0x7f || c == ctrl('H') => {
                if self.edit_index != self.write_index {
                    self.edit_index -= 1;
                    Self::backspace();
                }
            }
            _ => {
                if c == 0 {
                    return;
                }

                if self.edit_index - self.read_index >= Self::INPUT_BUF_LEN {
                    return;
                }

                let c = if c == '\r' as u8 { b'\n' } else { c };

                // echo back to the user.
                Self::putc(c);

                // store for consumption by consoleread().
                self.buf[self.edit_index % Self::INPUT_BUF_LEN] = c;
                self.edit_index += 1; // TODO: Overflow?

                if c == b'\n'
                    || c == ctrl('D')
                    || self.edit_index == self.read_index + Self::INPUT_BUF_LEN
                {
                    // wake up consoleread() if a whole line (or end-of-file)
                    // has arrived.
                    self.write_index = self.edit_index;
                    process::wakeup(&self.read_index as *const _ as usize);
                }
            }
        }
    }
}

impl<'a> SpinLockGuard<'a, Console> {
    //
    // user read()s from the console go here.
    // copy (up to) a whole input line to dst.
    // user_dist indicates whether dst is a user
    // or kernel address.
    //
    pub fn read(&mut self, mut dst: usize, mut n: usize) -> i32 {
        let target = n;
        while n > 0 {
            // wait until interrupt handler has put some
            // input into cons.buffer.
            while self.read_index == self.write_index {
                if process::is_killed() == Some(true) {
                    return -1;
                }
                process::sleep(&self.read_index as *const _ as usize, self)
            }

            let c = self.buf[self.read_index % Console::INPUT_BUF_LEN];
            self.read_index += 1; // TODO: Overflow?

            // end-of-file
            if c == ctrl('D') {
                if n < target {
                    // Save ^D for next time, to make sure
                    // caller gets a 0-byte result.
                    self.read_index -= 1;
                }
                break;
            }

            if !process::write_memory(dst, c) {
                break;
            }

            dst += 1;
            n -= 1;

            if c == b'\n' {
                // a whole line has arrived, return to
                // the user-level read().
                break;
            }
        }

        return (target - n) as i32;
    }
}

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for ch in s.chars() {
            Console::putc(ch as u8);
        }

        core::fmt::Result::Ok(())
    }
}

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console::new());

pub fn consoleintr(c: i32) {
    CONSOLE.lock().handle_interrupt(c as u8);
}

pub unsafe fn initialize() {
    UART::get().init();

    // connect read and write system calls
    // to consoleread and consolewrite.
    DEVICEFILES[1] = Some(DeviceFile {
        read: consoleread,
        write: consolewrite,
    });
}

fn consolewrite(src: usize, n: usize) -> i32 {
    Console::write(src, n) as i32
}

fn consoleread(dst: usize, n: usize) -> i32 {
    CONSOLE.lock().read(dst, n)
}
