/// ビット数を満たせる最小のバイト数を返す
pub const fn require_bytes(bits: usize) -> usize {
    (bits - 1) / 8 + 1
}

/// 指定ビットが存在するバイトのインデックスを返す
const fn byte_index(bit_index: usize) -> usize {
    bit_index / 8
}

/// 1バイト内の指定ビットに対するマスクを返す
const fn bit_mask(bit_index: usize) -> u8 {
    1 << (bit_index % 8)
}

/// ビットマップ
#[repr(C)]
#[derive(Clone, Debug)]
pub struct Bitmap<const BITS: usize>
where
    [u8; require_bytes(BITS)]:,
{
    bitmap: [u8; require_bytes(BITS)],
}

impl<const BITS: usize> Bitmap<BITS>
where
    [u8; require_bytes(BITS)]:,
{
    pub const fn new() -> Self {
        Self {
            bitmap: [0; require_bytes(BITS)],
        }
    }

    pub const fn bits(&self) -> usize {
        BITS
    }

    pub const fn bytes(&self) -> usize {
        require_bytes(BITS)
    }

    pub fn allocate(&mut self) -> Option<usize> {
        for i in 0..self.bits() {
            if self.get(i) == Some(false) {
                self.set(i, true).unwrap();
                return Some(i);
            }
        }
        None
    }

    pub const fn deallocate(&mut self, index: usize) -> Result<(), ()> {
        match self.get(index) {
            Some(true) => self.set(index, false),
            _ => Err(()),
        }
    }

    pub const fn get(&self, index: usize) -> Option<bool> {
        if index < BITS {
            Some(self.bitmap[byte_index(index)] & bit_mask(index) != 0)
        } else {
            None
        }
    }

    pub const fn set(&mut self, index: usize, value: bool) -> Result<(), ()> {
        if index < BITS {
            if value {
                self.bitmap[byte_index(index)] |= bit_mask(index);
            } else {
                self.bitmap[byte_index(index)] &= !bit_mask(index);
            }
            Ok(())
        } else {
            Err(())
        }
    }
}
