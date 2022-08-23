use core::ptr::NonNull;

struct Link<K> {
    index: usize,
    key: Option<K>,
    next: NonNull<Self>,
    prev: NonNull<Self>,
}

impl<K> Link<K> {
    const fn dangling() -> Self {
        Self {
            index: 0,
            key: None,
            next: NonNull::dangling(),
            prev: NonNull::dangling(),
        }
    }
}

pub struct Cache<K, const N: usize> {
    links: [Link<K>; N],
    head: NonNull<Link<K>>,
    tail: NonNull<Link<K>>,
}

impl<K, const N: usize> Cache<K, N> {
    pub const fn uninit() -> Self {
        Self {
            links: [const { Link::dangling() }; _],
            head: NonNull::dangling(),
            tail: NonNull::dangling(),
        }
    }

    pub fn init(&mut self) {
        for (i, link) in self.links.iter_mut().enumerate() {
            link.index = i;
        }

        unsafe {
            let first = NonNull::new_unchecked(&mut self.links[0]);
            self.head = first;
            self.tail = first;
            self.links[0].next = first;
            self.links[0].prev = first;

            for i in 1..N {
                let ptr = NonNull::new_unchecked(&mut self.links[i]);
                self.links[i].prev = self.tail;
                self.links[i].next = self.head;
                self.head.as_mut().prev = ptr;
                self.tail.as_mut().next = ptr;
                self.tail = ptr;
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = &mut Link<K>> {
        let mut next = Some(self.head);
        let end = self.tail;

        core::iter::from_fn(move || {
            let current = unsafe { next?.as_mut() };
            if next == Some(end) {
                next = None
            } else {
                next = Some(current.next);
            }
            Some(current)
        })
    }

    fn iter_rev(&self) -> impl Iterator<Item = &mut Link<K>> {
        let mut next = Some(self.tail);
        let end = self.head;

        core::iter::from_fn(move || {
            let current = unsafe { next?.as_mut() };
            if next == Some(end) {
                next = None
            } else {
                next = Some(current.prev);
            }
            Some(current)
        })
    }

    pub fn get(&mut self, key: K) -> Option<(usize, bool)>
    where
        K: PartialEq,
    {
        for link in self.iter() {
            if link.key.as_ref() == Some(&key) {
                return Some((link.index, false));
            }
        }

        for link in self.iter_rev() {
            if link.key.is_none() {
                link.key = Some(key);
                return Some((link.index, true));
            }
        }

        None
    }

    pub fn release(&mut self, index: usize) -> Option<()> {
        let link = self.links.get_mut(index)?;
        link.key = None;

        let this = unsafe { NonNull::new_unchecked(link) };
        if self.head.as_ptr() == link {
            self.head = link.next;
            self.tail = this;
        } else if self.tail.as_ptr() == link {
            // do nothing
        } else {
            unsafe {
                link.next.as_mut().prev = link.prev;
                link.prev.as_mut().next = link.next;
                link.next = self.head;
                link.prev = self.tail;

                self.head.as_mut().prev = this;
                self.tail.as_mut().next = this;
                self.tail = this;
            }
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
    pub const fn uninit() -> Self {
        Self {
            cache: Cache::uninit(),
            counts: [0; N],
            pinned: [false; N],
        }
    }

    pub fn init(&mut self) {
        self.cache.init()
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
