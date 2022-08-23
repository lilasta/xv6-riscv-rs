struct Link<K> {
    index: usize,
    next: usize,
    prev: usize,
    key: Option<K>,
}

impl<K> Link<K> {
    const fn dangling() -> Self {
        Self {
            index: 0,
            next: 0,
            prev: 0,
            key: None,
        }
    }
}

pub struct Cache<K, const N: usize> {
    links: [Link<K>; N],
    head: usize,
    tail: usize,
}

impl<K, const N: usize> Cache<K, N> {
    pub const fn new() -> Self {
        let mut this = Self {
            links: [const { Link::dangling() }; _],
            head: 0,
            tail: 0,
        };

        let mut i = 0;
        while i < N {
            this.links[i].index = i;
            i += 1;
        }

        this.head = 0;
        this.tail = 0;
        this.links[0].next = 0;
        this.links[0].prev = 0;

        let mut i = 1;
        while i < N {
            this.links[i].prev = this.tail;
            this.links[i].next = this.head;
            this.links[this.head].prev = i;
            this.links[this.tail].next = i;
            this.tail = i;
            i += 1;
        }

        this
    }

    fn indexes<'a>(&'a self) -> impl 'a + Iterator<Item = usize> {
        core::iter::successors(Some(self.head), |current| {
            if *current == self.tail {
                None
            } else {
                Some(self.links[*current].next)
            }
        })
    }

    fn indexes_rev<'a>(&'a self) -> impl 'a + Iterator<Item = usize> {
        core::iter::successors(Some(self.tail), |current| {
            if *current == self.head {
                None
            } else {
                Some(self.links[*current].prev)
            }
        })
    }

    fn find(&self, key: &K) -> Option<usize>
    where
        K: PartialEq,
    {
        for i in self.indexes() {
            if self.links[i].key.as_ref() == Some(key) {
                return Some(i);
            }
        }
        None
    }

    fn find_unused(&self) -> Option<usize>
    where
        K: PartialEq,
    {
        for i in self.indexes_rev() {
            if self.links[i].key.is_none() {
                return Some(i);
            }
        }
        None
    }

    pub fn get(&mut self, key: K) -> Option<(usize, bool)>
    where
        K: PartialEq,
    {
        if let Some(i) = self.find(&key) {
            return Some((i, false));
        }

        if let Some(i) = self.find_unused() {
            self.links[i].key = Some(key);
            return Some((i, true));
        }

        None
    }

    pub fn release(&mut self, index: usize) -> Option<()> {
        if index >= self.links.len() {
            return None;
        }

        self.links[index].key = None;

        if self.head == index {
            self.head = self.links[index].next;
            self.tail = index;
        } else if self.tail == index {
            // do nothing
        } else {
            let Link { next, prev, .. } = self.links[index];

            self.links[next].prev = prev;
            self.links[prev].next = next;
            self.links[index].next = self.head;
            self.links[index].prev = self.tail;

            self.links[self.head].prev = index;
            self.links[self.tail].next = index;
            self.tail = index;
        }

        Some(())
    }
}

pub struct CacheRc<K, const N: usize> {
    cache: Cache<K, N>,
    counts: [usize; N],
    pinned: [bool; N],
}

impl<K, const N: usize> CacheRc<K, N> {
    pub const fn new() -> Self {
        Self {
            cache: Cache::new(),
            counts: [0; N],
            pinned: [false; N],
        }
    }

    pub fn get(&mut self, key: K) -> Option<(usize, bool)>
    where
        K: PartialEq,
    {
        let found = self.cache.get(key)?;
        self.counts[found.0] += 1;
        Some(found)
    }

    pub fn release(&mut self, index: usize) -> Option<bool> {
        let count = self.counts.get_mut(index)?;
        match count {
            0 => unreachable!(),
            1 => {
                *count = 0;
                self.cache.release(index).unwrap();
                Some(true)
            }
            _ => {
                *count -= 1;
                Some(false)
            }
        }
    }

    // TODO: PinGuard?
    pub const fn pin(&mut self, index: usize) -> Option<()> {
        let count = self.counts.get_mut(index)?;
        let pin = self.pinned.get_mut(index)?;

        assert!(*pin == false);

        *count += 1;
        *pin = true;

        Some(())
    }

    pub const fn unpin(&mut self, index: usize) -> Option<()> {
        let count = self.counts.get_mut(index)?;
        let pin = self.pinned.get_mut(index)?;

        assert!(*count > 1);
        assert!(*pin == true);

        *count -= 1;
        *pin = false;

        Some(())
    }

    pub const fn reference_count(&mut self, index: usize) -> Option<usize> {
        Some(*self.counts.get_mut(index)?)
    }
}
