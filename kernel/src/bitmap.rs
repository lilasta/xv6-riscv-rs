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

    pub const fn get(&self, index: usize) -> Option<bool> {
        if index < BITS {
            Some(self.bitmap[byte_index(index)] & bit_mask(index) != 0)
        } else {
            None
        }
    }

    pub fn set(&mut self, index: usize, value: bool) -> Result<(), ()> {
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

#[test]
fn test_length_1() {
    let mut bitmap = Bitmap::<1>::new();
    assert!(bitmap.get(0) == Some(false));
    assert!(bitmap.get(1) == None);
    assert!(bitmap.set(0, false).is_ok());
    assert!(bitmap.set(0, true).is_ok());
    assert!(bitmap.set(1, true).is_err());
    assert!(bitmap.get(0) == Some(true));
}
