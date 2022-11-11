use crate::const_for;

struct Link<Key> {
    index: usize,
    next: usize,
    prev: usize,
    key: Option<Key>,
}

impl<Key> Link<Key> {
    const fn dangling() -> Self {
        Self {
            index: 0,
            next: 0,
            prev: 0,
            key: None,
        }
    }
}

pub struct Cache<Key, const N: usize> {
    links: [Link<Key>; N],
    head: usize,
    tail: usize,
}

impl<Key, const N: usize> Cache<Key, N> {
    pub const fn new() -> Self {
        let mut this = Self {
            links: [const { Link::dangling() }; _],
            head: 0,
            tail: 0,
        };

        const_for!(i in (0, N) {
            this.links[i].index = i;
        });

        // +---------+
        // |         |
        // +-->[0]---+
        this.links[0].next = 0;
        this.links[0].prev = 0;

        const_for!(i in (1, N) {
            // +--------- ... <--------+
            // |                       |
            // +->tail--->[i]--->head--+
            this.links[i].prev = this.tail;
            this.links[i].next = this.head;
            this.links[this.head].prev = i;
            this.links[this.tail].next = i;

            this.tail = i;
        });

        this
    }

    fn indexes(&self) -> impl '_ + Iterator<Item = usize> {
        core::iter::successors(Some(self.head), |current| {
            if *current == self.tail {
                None
            } else {
                Some(self.links[*current].next)
            }
        })
    }

    fn indexes_rev(&self) -> impl '_ + Iterator<Item = usize> {
        core::iter::successors(Some(self.tail), |current| {
            if *current == self.head {
                None
            } else {
                Some(self.links[*current].prev)
            }
        })
    }

    fn find(&self, key: &Key) -> Option<usize>
    where
        Key: PartialEq,
    {
        self.indexes()
            .find(|i| self.links[*i].key.as_ref() == Some(key))
    }

    fn find_unused(&self) -> Option<usize>
    where
        Key: PartialEq,
    {
        self.indexes_rev().find(|i| self.links[*i].key.is_none())
    }

    pub fn get(&mut self, key: Key) -> Option<(usize, bool)>
    where
        Key: PartialEq,
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

    pub fn remove(&mut self, index: usize) -> Option<()> {
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

pub struct CacheRc<Key, const N: usize> {
    cache: Cache<Key, N>,
    counts: [usize; N],
}

impl<Key, const N: usize> CacheRc<Key, N> {
    pub const fn new() -> Self {
        Self {
            cache: Cache::new(),
            counts: [0; N],
        }
    }

    pub fn get(&mut self, key: Key) -> Option<(usize, bool)>
    where
        Key: PartialEq,
    {
        let found = self.cache.get(key)?;
        self.counts[found.0] += 1;
        Some(found)
    }

    pub const fn duplicate(&mut self, index: usize) -> Option<usize> {
        const fn increment_count(count: &mut usize) {
            *count += 1;
        }

        self.counts
            .get_mut(index)
            .map(increment_count)
            .and(Some(index))
    }

    pub fn release(&mut self, index: usize) -> Option<bool> {
        let count = self.counts.get_mut(index)?;
        match count {
            0 => unreachable!(),
            1 => {
                *count = 0;
                self.cache.remove(index).unwrap();
                Some(true)
            }
            _ => {
                *count -= 1;
                Some(false)
            }
        }
    }

    pub const fn reference_count(&self, index: usize) -> Option<usize> {
        self.counts.get(index).cloned()
    }
}
