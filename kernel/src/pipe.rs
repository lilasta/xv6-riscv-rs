pub struct Pipe<const SIZE: usize> {
    data: [u8; SIZE],
    read: usize,
    write: usize,
    is_reading: bool,
    is_writing: bool,
}

impl<const SIZE: usize> Pipe<SIZE> {
    pub const fn new() -> Self {
        Self {
            data: [0; _],
            read: 0,
            write: 0,
            is_reading: false,
            is_writing: false,
        }
    }

    pub fn write(&mut self, addr: usize, n: usize) -> usize {
        todo!()
    }

    pub fn read(&mut self, addr: usize, n: usize) -> usize {
        todo!()
    }
}
