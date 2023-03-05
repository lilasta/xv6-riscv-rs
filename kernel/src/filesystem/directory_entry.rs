#[repr(C)]
#[derive(Debug)]
pub struct DirectoryEntry {
    inode_number: u16,
    name: [u8; Self::NAME_LENGTH],
}

impl DirectoryEntry {
    pub const NAME_LENGTH: usize = 14;

    pub const fn unused() -> Self {
        Self {
            inode_number: 0,
            name: [0; _],
        }
    }

    pub fn new(inode_number: usize, name: &str) -> Self {
        let mut this = Self {
            inode_number: inode_number as u16,
            name: [0; _],
        };

        let len = name.len().min(DirectoryEntry::NAME_LENGTH);
        this.name[..len].copy_from_slice(&name.as_bytes()[..len]);

        this
    }

    pub const fn inode_number(&self) -> usize {
        self.inode_number as usize
    }

    pub fn is(&self, name: &str) -> bool {
        let len = name.len().min(DirectoryEntry::NAME_LENGTH);
        &self.name[..len] == &name.as_bytes()[..len]
    }
}
