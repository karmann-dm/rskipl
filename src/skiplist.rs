//! Lock-free, arena backed MVCC skiplist
//! ordering is key ASC, version DESC
//!
//! Node layout:
//! +0 version   u64 immutable after publish
//! +8 value     AtomicU64 (offset << 32) | len ; len == u32::max => tombstone
//! +16 key_off  u32 immutable after publish
//! +20 key_len  u32 immutable after publish
//! +24 tower    [AtomicU32; height] only height slots are allocated

use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arena::{Arena, ArenaFull, NULL};

// 20 with p=1/4 branching handles ~4^20 entries, far past any memtable
pub const MAX_HEIGHT: usize = 20;

const OFF_VERSION: u32 = 0;
const OFF_VALUE: u32 = 8;
const OFF_KEY_OFF: u32 = 16;
const OFF_KEY_LEN: u32 = 20;
const OFF_TOWER: u32 = 24;
const NODE_ALIGH: u32 = 8;

const TOMBSTONE: u32 = u32::MAX;

pub const MAX_VERSION: u64 = (1 << 56) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Arena is full. The caller freezes this memtable and retries on a new one
    Full,
    /// Keys and values are lenght-prefixed with u32
    TooLarge,
}

impl From<ArenaFull> for Error {
    fn from(_: ArenaFull) -> Self {
        Error::Full
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub key: &'a [u8],
    pub version: u64,
    /// None is a tombstone, which is distinct from key absent. The read path must stop descending to older levels when it sees one
    pub value: Option<&'a [u8]>,
}

#[inline]
fn encode_value(offset: u32, len: u32) -> u64 {
    ((offset as u64) << 32) | (len as u64)
}

#[inline]
fn decode_value(word: u64) -> (u32, u32) {
    ((word >> 32) as u32, word as u32)
}

pub struct SkipList {
    arena: Arena,
    head: u32,
    /// Highest tower currently in use. Lets search skip empty upper levels
    height: AtomicU32,
    len: AtomicU32,
    /// Height RNG state. Bumped per call, so no thread local or rand needed
    seed: AtomicU64,
}

// all shared mutable state is atomic; the arena is Send + Sync
unsafe impl Send for SkipList {}
unsafe impl Sync for SkipList {}

impl SkipList {
    pub fn new(capacity: u32) -> Result<Self, Error> {
        let arena = Arena::new(capacity);
        let head = alloc_node(&arena, MAX_HEIGHT, 0, NULL, 0, encode_value(0, 0))?;
        Ok(Self {
            arena,
            head,
            height: AtomicU32::new(1),
            len: AtomicU32::new(0),
            seed: AtomicU64::new(0x2545_F491_4F6C_DD1D),
        })
    }

    #[inline]
    unsafe fn version_of(&self, node: u32) -> u64 {
        unsafe { (self.arena.ptr_at(node + OFF_VERSION) as *const u64).read() }
    }

    #[inline]
    unsafe fn value_cell(&self, node: u32) -> &AtomicU64 {
        let p = unsafe { self.arena.ptr_at(node + OFF_VALUE) };
        debug_assert!((p as usize).is_multiple_of(8));
        unsafe { &*(p as *const AtomicU64) }
    }

    #[inline]
    unsafe fn tower(&self, node: u32, level: usize) -> &AtomicU32 {
        let p = unsafe { self.arena.ptr_at(node + OFF_TOWER + (level as u32) * 4) };
        debug_assert!((p as usize).is_multiple_of(4));
        unsafe { &*(p as *const AtomicU32) }
    }

    #[inline]
    unsafe fn key_of(&self, node: u32) -> &[u8] {
        let off = unsafe { (self.arena.ptr_at(node + OFF_KEY_OFF) as *const u32).read() };
        let len = unsafe { (self.arena.ptr_at(node + OFF_KEY_LEN) as *const u32).read() };
        unsafe { self.arena.slice(off, len) }
    }

    /// Order a node against a target key,version
    /// versions descend with the key
    #[inline]
    unsafe fn cmp_node(&self, node: u32, key: &[u8], version: u64) -> CmpOrdering {
        unsafe { self.key_of(node) }
            .cmp(key)
            .then_with(|| version.cmp(&unsafe { self.version_of(node) }))
    }

    #[inline]
    unsafe fn find_splice_for_level(
        &self,
        key: &[u8],
        version: u64,
        start: u32,
        level: usize,
    ) -> (u32, u32) {
        let mut prev = start;
        loop {
            let next = unsafe { self.tower(prev, level) }.load(Ordering::Acquire);
            if next == NULL {
                return (prev, NULL);
            }
            match unsafe { self.cmp_node(next, key, version) } {
                CmpOrdering::Less => prev = next,
                _ => return (prev, next),
            }
        }
    }

    fn random_height(&self) -> usize {
        let mut x = self
            .seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;

        let mut h = 1;
        while h < MAX_HEIGHT && (x & 3) == 0 {
            h += 1;
            x >>= 2;
        }
        h
    }

    // write
    pub fn insert(&self, version: u64, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.put(version, key, Some(value))
    }

    pub fn remove(&self, version: u64, key: &[u8]) -> Result<(), Error> {
        self.put(version, key, None)
    }

    fn put(&self, version: u64, key: &[u8], value: Option<&[u8]>) -> Result<(), Error> {
        if key.len() > u32::MAX as usize || value.is_some_and(|v| v.len() >= TOMBSTONE as usize) {
            return Err(Error::TooLarge);
        }
        debug_assert!(version <= MAX_VERSION);

        let mut prev = [self.head; MAX_HEIGHT + 1];
        let mut next = [NULL; MAX_HEIGHT + 1];

        unsafe {
            // descend from the top, recording the splice at each level
            for i in (0..MAX_HEIGHT).rev() {
                let (p, n) = self.find_splice_for_level(key, version, prev[i + 1], i);
                prev[i] = p;
                next[i] = n;

                // exact key,version hit; upsert in place. No new node, no arena consumed beyond the value bytes
                if n != NULL && self.cmp_node(n, key, version) == CmpOrdering::Equal {
                    let word = self.encode_new_value(value)?;
                    self.value_cell(n).store(word, Ordering::Release);
                    return Ok(());
                }
            }

            let key_off = self.arena.alloc_bytes(key)?;
            let value_word = self.encode_new_value(value)?;
            let height = self.random_height();
            let node = alloc_node(
                &self.arena,
                height,
                version,
                key_off,
                key.len() as u32,
                value_word,
            )?;

            // Raise the list height so searches start high enough to find us
            let mut cur = self.height.load(Ordering::Relaxed);
            while (height as u32) > cur {
                match self.height.compare_exchange_weak(
                    cur,
                    height as u32,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }

            // Link bottom-up
            for i in 0..height {
                loop {
                    self.tower(node, i).store(next[i], Ordering::Relaxed);

                    match self.tower(prev[i], i).compare_exchange(
                        next[i],
                        node,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(_) => {
                            // A concurrent insert landed inside our splice
                            // re-find at this level only, levels below stay valid
                            let (p, n) = self.find_splice_for_level(key, version, prev[i], i);
                            prev[i] = p;
                            next[i] = n;

                            if i == 0
                                && n != NULL
                                && self.cmp_node(n, key, version) == CmpOrdering::Equal
                            {
                                let word = self.value_cell(node).load(Ordering::Relaxed);
                                self.value_cell(n).store(word, Ordering::Release);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        self.len.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn encode_new_value(&self, value: Option<&[u8]>) -> Result<u64, Error> {
        Ok(match value {
            None => encode_value(0, TOMBSTONE),
            Some(v) => {
                let off = self.arena.alloc_bytes(v)?;
                encode_value(off, v.len() as u32)
            }
        })
    }

    // read path
    pub fn get(&self, snapshot: u64, key: &[u8]) -> Option<Entry<'_>> {
        unsafe {
            let mut prev = self.head;
            let top = self.height.load(Ordering::Acquire) as usize;
            for level in (0..top).rev() {
                let (p, _) = self.find_splice_for_level(key, snapshot, prev, level);
                prev = p;
            }

            let node = self.tower(prev, 0).load(Ordering::Acquire);
            if node == NULL || self.key_of(node) != key {
                return None;
            }
            Some(self.entry(node))
        }
    }

    #[inline]
    unsafe fn entry(&self, node: u32) -> Entry<'_> {
        let (voff, vlen) = decode_value(unsafe { self.value_cell(node) }.load(Ordering::Acquire));
        Entry {
            key: unsafe { self.key_of(node) },
            version: unsafe { self.version_of(node) },
            value: if vlen == TOMBSTONE {
                None
            } else {
                Some(unsafe { self.arena.slice(voff, vlen) })
            },
        }
    }

    pub fn iter_all(&self) -> Iter<'_> {
        Iter {
            list: self,
            node: self.head,
        }
    }

    #[inline]
    pub fn allocated(&self) -> usize {
        self.arena.allocated()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.arena.remaining()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed) as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn alloc_node(
    arena: &Arena,
    height: usize,
    version: u64,
    key_off: u32,
    key_len: u32,
    value: u64,
) -> Result<u32, Error> {
    let size = OFF_TOWER + (height as u32) * 4;
    let node = arena.alloc(size, NODE_ALIGH)?;
    unsafe {
        (arena.ptr_at(node + OFF_VERSION) as *mut u64).write(version);
        (arena.ptr_at(node + OFF_VALUE) as *mut u64).write(value);
        (arena.ptr_at(node + OFF_KEY_OFF) as *mut u32).write(key_off);
        (arena.ptr_at(node + OFF_KEY_LEN) as *mut u32).write(key_len);
        for level in 0..height {
            (arena.ptr_at(node + OFF_TOWER + (level as u32) * 4) as *mut u32).write(NULL);
        }
    }
    Ok(node)
}

pub struct Iter<'a> {
    list: &'a SkipList,
    node: u32,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let list = self.list;
        unsafe {
            let next = list.tower(self.node, 0).load(Ordering::Acquire);
            if next == NULL {
                return None;
            }
            self.node = next;
            Some(list.entry(next))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    const CAP: u32 = 1 << 22;

    fn list() -> SkipList {
        SkipList::new(CAP).unwrap()
    }

    fn key(i: usize) -> Vec<u8> {
        format!("key{:06}", i).into_bytes()
    }

    /// What a read at a snapshot resolved to. Keeps "no visible version" and
    /// "visible tombstone" apart, since the compaction path depends on that.
    #[derive(Debug, PartialEq, Eq)]
    enum Read {
        Missing,
        Tombstone(u64),
        Live(u64, Vec<u8>),
    }

    fn read(list: &SkipList, snapshot: u64, k: &[u8]) -> Read {
        match list.get(snapshot, k) {
            None => Read::Missing,
            Some(e) => {
                assert_eq!(e.key, k, "get returned an entry for a different key");
                assert!(
                    e.version <= snapshot,
                    "get returned version {} above snapshot {}",
                    e.version,
                    snapshot
                );
                match e.value {
                    None => Read::Tombstone(e.version),
                    Some(v) => Read::Live(e.version, v.to_vec()),
                }
            }
        }
    }

    fn live(version: u64, v: &[u8]) -> Read {
        Read::Live(version, v.to_vec())
    }

    /// xorshift64, so the randomized tests are reproducible without a dev-dependency
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed | 1)
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    // ---------------------------------------------------------------- basics

    #[test]
    fn empty_list_reads_nothing() {
        let l = list();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
        assert_eq!(read(&l, MAX_VERSION, b"anything"), Read::Missing);
        assert_eq!(l.iter_all().count(), 0);
    }

    #[test]
    fn insert_then_get() {
        let l = list();
        l.insert(1, b"k", b"v").unwrap();
        assert_eq!(read(&l, 1, b"k"), live(1, b"v"));
        assert_eq!(l.len(), 1);
        assert!(!l.is_empty());
    }

    #[test]
    fn get_of_absent_key_is_missing() {
        let l = list();
        l.insert(1, b"b", b"vb").unwrap();
        assert_eq!(read(&l, 1, b"a"), Read::Missing);
        assert_eq!(read(&l, 1, b"c"), Read::Missing);
    }

    #[test]
    fn get_does_not_match_on_prefix() {
        let l = list();
        l.insert(1, b"abc", b"v").unwrap();
        assert_eq!(read(&l, 1, b"ab"), Read::Missing);
        assert_eq!(read(&l, 1, b"abcd"), Read::Missing);
        assert_eq!(read(&l, 1, b"abc"), live(1, b"v"));
    }

    #[test]
    fn empty_key_and_empty_value_round_trip() {
        let l = list();
        l.insert(1, b"", b"v").unwrap();
        l.insert(1, b"k", b"").unwrap();
        assert_eq!(read(&l, 1, b""), live(1, b"v"));
        assert_eq!(read(&l, 1, b"k"), live(1, b""));
        // an empty value is emphatically not a tombstone
        assert_ne!(read(&l, 1, b"k"), Read::Tombstone(1));
    }

    #[test]
    fn keys_come_back_sorted_regardless_of_insert_order() {
        let l = list();
        let mut rng = Rng::new(7);
        let mut expected: Vec<Vec<u8>> = (0..500).map(key).collect();
        let mut order: Vec<usize> = (0..500).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i as u64 + 1) as usize);
        }
        for i in order {
            l.insert(1, &key(i), &key(i)).unwrap();
        }
        expected.sort();
        let got: Vec<Vec<u8>> = l.iter_all().map(|e| e.key.to_vec()).collect();
        assert_eq!(got, expected);
        assert_eq!(l.len(), 500);
    }

    #[test]
    fn binary_key_ordering_is_bytewise() {
        let l = list();
        for k in [&b"a"[..], b"ab", b"b", b"", b"\x00", b"\xff", b"a\x00"] {
            l.insert(1, k, b"v").unwrap();
        }
        let got: Vec<Vec<u8>> = l.iter_all().map(|e| e.key.to_vec()).collect();
        let mut want: Vec<Vec<u8>> = [&b"a"[..], b"ab", b"b", b"", b"\x00", b"\xff", b"a\x00"]
            .iter()
            .map(|k| k.to_vec())
            .collect();
        want.sort();
        assert_eq!(got, want);
    }

    // ---------------------------------------------------------------- MVCC

    #[test]
    fn snapshot_sees_newest_version_at_or_below_it() {
        let l = list();
        l.insert(10, b"k", b"v10").unwrap();
        l.insert(20, b"k", b"v20").unwrap();
        l.insert(30, b"k", b"v30").unwrap();

        assert_eq!(read(&l, 9, b"k"), Read::Missing);
        assert_eq!(read(&l, 10, b"k"), live(10, b"v10"));
        assert_eq!(read(&l, 19, b"k"), live(10, b"v10"));
        assert_eq!(read(&l, 20, b"k"), live(20, b"v20"));
        assert_eq!(read(&l, 29, b"k"), live(20, b"v20"));
        assert_eq!(read(&l, 30, b"k"), live(30, b"v30"));
        assert_eq!(read(&l, MAX_VERSION, b"k"), live(30, b"v30"));
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn versions_are_written_out_newest_first() {
        let l = list();
        // insert out of order on purpose
        l.insert(20, b"k", b"v20").unwrap();
        l.insert(30, b"k", b"v30").unwrap();
        l.insert(10, b"k", b"v10").unwrap();
        l.insert(15, b"j", b"j15").unwrap();

        let got: Vec<(Vec<u8>, u64)> = l.iter_all().map(|e| (e.key.to_vec(), e.version)).collect();
        assert_eq!(
            got,
            vec![
                (b"j".to_vec(), 15),
                (b"k".to_vec(), 30),
                (b"k".to_vec(), 20),
                (b"k".to_vec(), 10),
            ]
        );
    }

    #[test]
    fn version_zero_is_a_usable_version() {
        let l = list();
        l.insert(0, b"k", b"v0").unwrap();
        assert_eq!(read(&l, 0, b"k"), live(0, b"v0"));
        assert_eq!(read(&l, MAX_VERSION, b"k"), live(0, b"v0"));
    }

    #[test]
    fn max_version_is_a_usable_version() {
        let l = list();
        l.insert(MAX_VERSION, b"k", b"top").unwrap();
        l.insert(1, b"k", b"bottom").unwrap();
        assert_eq!(read(&l, MAX_VERSION, b"k"), live(MAX_VERSION, b"top"));
        assert_eq!(read(&l, MAX_VERSION - 1, b"k"), live(1, b"bottom"));
    }

    #[test]
    fn same_version_on_same_key_upserts_in_place() {
        let l = list();
        l.insert(5, b"k", b"first").unwrap();
        assert_eq!(l.len(), 1);
        let allocated_after_first = l.allocated();

        l.insert(5, b"k", b"second").unwrap();
        assert_eq!(read(&l, 5, b"k"), live(5, b"second"));
        assert_eq!(l.len(), 1, "an upsert must not create a second node");
        assert_eq!(l.iter_all().count(), 1);

        // only the value bytes should have been consumed, not a whole node
        let grew_by = l.allocated() - allocated_after_first;
        assert!(
            grew_by <= b"second".len() + 8,
            "upsert allocated {} bytes, expected roughly the value only",
            grew_by
        );
    }

    #[test]
    fn upsert_reaches_a_node_found_above_level_zero() {
        // force enough entries that some node lives high in the tower, then
        // rewrite every one of them at the same version
        let l = list();
        for i in 0..2000 {
            l.insert(1, &key(i), b"a").unwrap();
        }
        for i in 0..2000 {
            l.insert(1, &key(i), b"b").unwrap();
        }
        assert_eq!(l.len(), 2000);
        for i in 0..2000 {
            assert_eq!(read(&l, 1, &key(i)), live(1, b"b"));
        }
    }

    // ---------------------------------------------------------------- tombstones

    #[test]
    fn tombstone_hides_older_live_version() {
        let l = list();
        l.insert(1, b"k", b"v1").unwrap();
        l.remove(2, b"k").unwrap();

        assert_eq!(read(&l, 1, b"k"), live(1, b"v1"));
        assert_eq!(read(&l, 2, b"k"), Read::Tombstone(2));
        assert_eq!(read(&l, MAX_VERSION, b"k"), Read::Tombstone(2));
        assert_eq!(l.len(), 2, "a tombstone is its own version");
    }

    #[test]
    fn resurrect_after_tombstone() {
        let l = list();
        l.insert(1, b"k", b"v1").unwrap();
        l.remove(2, b"k").unwrap();
        l.insert(3, b"k", b"v3").unwrap();

        assert_eq!(read(&l, 1, b"k"), live(1, b"v1"));
        assert_eq!(read(&l, 2, b"k"), Read::Tombstone(2));
        assert_eq!(read(&l, 3, b"k"), live(3, b"v3"));
    }

    #[test]
    fn tombstone_on_a_key_that_was_never_written() {
        let l = list();
        l.remove(1, b"ghost").unwrap();
        assert_eq!(read(&l, 1, b"ghost"), Read::Tombstone(1));
        assert_eq!(read(&l, 0, b"ghost"), Read::Missing);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn tombstone_can_be_overwritten_by_a_live_value_at_the_same_version() {
        let l = list();
        l.remove(4, b"k").unwrap();
        assert_eq!(read(&l, 4, b"k"), Read::Tombstone(4));
        l.insert(4, b"k", b"back").unwrap();
        assert_eq!(read(&l, 4, b"k"), live(4, b"back"));
        l.remove(4, b"k").unwrap();
        assert_eq!(read(&l, 4, b"k"), Read::Tombstone(4));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn tombstone_on_one_key_does_not_shadow_its_neighbours() {
        let l = list();
        l.insert(1, b"a", b"va").unwrap();
        l.insert(1, b"b", b"vb").unwrap();
        l.insert(1, b"c", b"vc").unwrap();
        l.remove(2, b"b").unwrap();

        assert_eq!(read(&l, 2, b"a"), live(1, b"va"));
        assert_eq!(read(&l, 2, b"b"), Read::Tombstone(2));
        assert_eq!(read(&l, 2, b"c"), live(1, b"vc"));
    }

    // ---------------------------------------------------------------- capacity

    #[test]
    fn arena_exhaustion_is_reported_not_panicked() {
        let l = SkipList::new(4096).unwrap();
        let mut written = Vec::new();
        let mut i = 0;
        let err = loop {
            assert!(i < 100_000, "a 4 KiB arena should have filled by now");
            match l.insert(1, &key(i), b"0123456789abcdef") {
                Ok(()) => written.push(i),
                Err(e) => break e,
            }
            i += 1;
        };
        assert_eq!(err, Error::Full);
        assert!(!written.is_empty(), "nothing fit at all");

        // everything that was accepted before the failure is still readable
        for i in &written {
            assert_eq!(read(&l, 1, &key(*i)), live(1, b"0123456789abcdef"));
        }
        assert_eq!(l.iter_all().count(), written.len());
        assert!(l.remaining() < l.capacity());
    }

    #[test]
    fn a_full_arena_stays_full() {
        let l = SkipList::new(4096).unwrap();
        let mut i = 0;
        while l.insert(1, &key(i), b"0123456789abcdef").is_ok() {
            i += 1;
        }
        for _ in 0..10 {
            assert_eq!(l.insert(1, b"x", b"y"), Err(Error::Full));
        }
    }

    #[test]
    fn capacity_accounting() {
        let l = list();
        assert_eq!(l.capacity(), CAP as usize);
        let before = l.allocated();
        l.insert(1, b"k", b"v").unwrap();
        assert!(l.allocated() > before);
        assert_eq!(l.allocated() + l.remaining(), l.capacity());
    }

    // ---------------------------------------------------------------- height / RNG

    #[test]
    fn random_height_is_in_range() {
        let l = list();
        for _ in 0..10_000 {
            let h = l.random_height();
            assert!((1..=MAX_HEIGHT).contains(&h), "height {} out of range", h);
        }
    }

    #[test]
    fn random_height_actually_varies() {
        let l = list();
        let mut seen = BTreeSet::new();
        for _ in 0..10_000 {
            seen.insert(l.random_height());
        }
        assert!(
            seen.len() >= 4,
            "only saw heights {:?} in 10k draws; the height RNG is not advancing",
            seen
        );
    }

    #[test]
    fn random_height_follows_the_quarter_branching_factor() {
        let l = list();
        const N: usize = 100_000;
        let mut counts = [0usize; MAX_HEIGHT + 1];
        for _ in 0..N {
            counts[l.random_height()] += 1;
        }
        let p1 = counts[1] as f64 / N as f64;
        let p2 = counts[2] as f64 / N as f64;
        assert!(
            (p1 - 0.75).abs() < 0.03,
            "P(height==1) was {:.3}, expected ~0.75 (counts {:?})",
            p1,
            &counts[..6]
        );
        assert!(
            (p2 - 0.1875).abs() < 0.03,
            "P(height==2) was {:.3}, expected ~0.1875",
            p2
        );
    }

    #[test]
    fn list_height_grows_with_the_entry_count() {
        let l = list();
        assert_eq!(l.height.load(Ordering::Relaxed), 1);
        for i in 0..20_000 {
            l.insert(1, &key(i), b"v").unwrap();
        }
        let h = l.height.load(Ordering::Relaxed);
        // log4(20000) is about 7; anything below 4 means towers are not being built
        assert!(
            h >= 4,
            "list height is {} after 20k inserts, so the index levels are not doing any work",
            h
        );
        assert!(h as usize <= MAX_HEIGHT);
    }

    // ---------------------------------------------------------------- model check

    #[test]
    fn matches_a_btreemap_model() {
        const KEYS: u64 = 40;
        const VERSIONS: u64 = 12;

        let l = list();
        let mut model: BTreeMap<(Vec<u8>, u64), Option<Vec<u8>>> = BTreeMap::new();
        let mut rng = Rng::new(0xDEAD_BEEF);

        for step in 0..8_000u64 {
            let k = key(rng.below(KEYS) as usize);
            let v = rng.below(VERSIONS) + 1;
            if rng.below(4) == 0 {
                l.remove(v, &k).unwrap();
                model.insert((k, v), None);
            } else {
                let payload =
                    format!("{}@{}#{}", String::from_utf8_lossy(&k), v, step).into_bytes();
                l.insert(v, &k, &payload).unwrap();
                model.insert((k, v), Some(payload));
            }
        }

        for ki in 0..KEYS {
            let k = key(ki as usize);
            for snapshot in 0..=VERSIONS + 1 {
                let want = model
                    .range((k.clone(), 0)..=(k.clone(), snapshot))
                    .next_back()
                    .map(|((_, ver), val)| match val {
                        None => Read::Tombstone(*ver),
                        Some(v) => Read::Live(*ver, v.clone()),
                    })
                    .unwrap_or(Read::Missing);
                assert_eq!(
                    read(&l, snapshot, &k),
                    want,
                    "mismatch for key {:?} at snapshot {}",
                    String::from_utf8_lossy(&k),
                    snapshot
                );
            }
        }

        // the full scan must agree with the model, in key ASC / version DESC order
        let scanned: Vec<(Vec<u8>, u64, Option<Vec<u8>>)> = l
            .iter_all()
            .map(|e| (e.key.to_vec(), e.version, e.value.map(|v| v.to_vec())))
            .collect();
        let mut expected: Vec<(Vec<u8>, u64, Option<Vec<u8>>)> = model
            .iter()
            .map(|((k, v), val)| (k.clone(), *v, val.clone()))
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        assert_eq!(scanned, expected);
        assert_eq!(l.len(), model.len());
    }

    // ---------------------------------------------------------------- concurrency

    #[test]
    fn concurrent_inserts_of_disjoint_keys() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 2_000;
        let l = SkipList::new(1 << 24).unwrap();
        let barrier = Barrier::new(THREADS);

        thread::scope(|s| {
            for t in 0..THREADS {
                let l = &l;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        // interleave the ranges so threads collide in the same region
                        let k = key(i * THREADS + t);
                        l.insert(1, &k, &k).unwrap();
                    }
                });
            }
        });

        let total = THREADS * PER_THREAD;
        assert_eq!(l.len(), total, "lost or duplicated nodes");
        for i in 0..total {
            let k = key(i);
            assert_eq!(read(&l, 1, &k), live(1, &k), "key {} went missing", i);
        }

        let keys: Vec<Vec<u8>> = l.iter_all().map(|e| e.key.to_vec()).collect();
        assert_eq!(keys.len(), total, "level 0 chain lost nodes");
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "level 0 chain is out of order"
        );
    }

    #[test]
    fn concurrent_versions_of_one_hot_key() {
        const THREADS: usize = 8;
        const PER_THREAD: u64 = 500;
        let l = SkipList::new(1 << 22).unwrap();
        let barrier = Barrier::new(THREADS);

        thread::scope(|s| {
            for t in 0..THREADS as u64 {
                let l = &l;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        let v = i * THREADS as u64 + t + 1;
                        l.insert(v, b"hot", format!("v{}", v).as_bytes()).unwrap();
                    }
                });
            }
        });

        let total = THREADS as u64 * PER_THREAD;
        assert_eq!(l.len(), total as usize);
        for v in 1..=total {
            assert_eq!(
                read(&l, v, b"hot"),
                live(v, format!("v{}", v).as_bytes()),
                "version {} not visible at its own snapshot",
                v
            );
        }
        let versions: Vec<u64> = l.iter_all().map(|e| e.version).collect();
        assert_eq!(versions.len(), total as usize);
        assert!(
            versions.windows(2).all(|w| w[0] > w[1]),
            "versions of one key are not strictly descending"
        );
    }

    #[test]
    fn concurrent_upserts_of_one_cell_never_tear() {
        const THREADS: usize = 8;
        let l = SkipList::new(1 << 22).unwrap();
        l.insert(1, b"k", b"init").unwrap();
        let barrier = Barrier::new(THREADS + 1);
        let stop = AtomicUsize::new(0);

        thread::scope(|s| {
            {
                let l = &l;
                let barrier = &barrier;
                let stop = &stop;
                s.spawn(move || {
                    barrier.wait();
                    while stop.load(Ordering::Relaxed) == 0 {
                        for _ in 0..1000 {
                            match read(l, 1, b"k") {
                                Read::Live(1, v) => {
                                    assert!(v == b"init" || v.len() == 9, "torn value: {:?}", v);
                                }
                                other => panic!("unexpected read {:?}", other),
                            }
                        }
                    }
                });
            }

            let writers: Vec<_> = (0..THREADS)
                .map(|t| {
                    let l = &l;
                    let barrier = &barrier;
                    s.spawn(move || {
                        barrier.wait();
                        for i in 0..2_000 {
                            l.insert(1, b"k", format!("{:04}-{:04}", t, i).as_bytes())
                                .unwrap();
                        }
                    })
                })
                .collect();
            for w in writers {
                w.join().unwrap();
            }
            stop.store(1, Ordering::Relaxed);
        });

        assert_eq!(l.len(), 1, "upserts created extra nodes");
        match read(&l, 1, b"k") {
            Read::Live(1, v) => assert_eq!(v.len(), 9),
            other => panic!("unexpected final read {:?}", other),
        }
    }

    #[test]
    fn readers_never_observe_a_half_linked_node() {
        // one writer appends versions of a set of keys; readers scan concurrently
        // and every entry they see must be internally consistent
        const KEYS: usize = 200;
        const ROUNDS: u64 = 200;
        let l = SkipList::new(1 << 24).unwrap();
        let done = AtomicUsize::new(0);
        let barrier = Barrier::new(5);

        thread::scope(|s| {
            {
                let l = &l;
                let done = &done;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    for v in 1..=ROUNDS {
                        for i in 0..KEYS {
                            let k = key(i);
                            let payload = format!("{}@{}", i, v).into_bytes();
                            l.insert(v, &k, &payload).unwrap();
                        }
                    }
                    done.store(1, Ordering::Release);
                });
            }

            for _ in 0..4 {
                let l = &l;
                let done = &done;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    let mut scans = 0u64;
                    while done.load(Ordering::Acquire) == 0 || scans < 5 {
                        let mut prev: Option<(Vec<u8>, u64)> = None;
                        for e in l.iter_all() {
                            let v = e.value.expect("no tombstones were written");
                            let idx: usize = std::str::from_utf8(e.key).unwrap()[3..]
                                .trim_start_matches('0')
                                .parse()
                                .unwrap_or(0);
                            assert_eq!(
                                v,
                                format!("{}@{}", idx, e.version).as_bytes(),
                                "value does not belong to its own key/version"
                            );
                            if let Some((pk, pv)) = prev {
                                let ok = pk < e.key.to_vec() || (pk == e.key && pv > e.version);
                                assert!(ok, "scan out of order at {:?}/{}", e.key, e.version);
                            }
                            prev = Some((e.key.to_vec(), e.version));
                        }
                        // a point read must agree with the scan
                        let k = key(KEYS / 2);
                        if let Read::Live(ver, val) = read(l, ROUNDS, &k) {
                            assert_eq!(val, format!("{}@{}", KEYS / 2, ver).into_bytes());
                        }
                        scans += 1;
                    }
                });
            }
        });

        assert_eq!(l.len(), KEYS * ROUNDS as usize);
        for i in 0..KEYS {
            assert_eq!(
                read(&l, ROUNDS, &key(i)),
                live(ROUNDS, format!("{}@{}", i, ROUNDS).as_bytes())
            );
        }
    }

    #[test]
    fn concurrent_mixed_writes_match_the_model() {
        const THREADS: usize = 6;
        const KEYS: u64 = 64;
        let l = SkipList::new(1 << 24).unwrap();
        let barrier = Barrier::new(THREADS);

        // each thread owns a version residue class, so no two threads write the
        // same (key, version) and the expected final state is exact
        thread::scope(|s| {
            for t in 0..THREADS as u64 {
                let l = &l;
                let barrier = &barrier;
                s.spawn(move || {
                    let mut rng = Rng::new(0x1234 + t);
                    barrier.wait();
                    for i in 0..1_000u64 {
                        let k = key(rng.below(KEYS) as usize);
                        let v = i * THREADS as u64 + t + 1;
                        if v.is_multiple_of(5) {
                            l.remove(v, &k).unwrap();
                        } else {
                            l.insert(v, &k, format!("{}", v).as_bytes()).unwrap();
                        }
                    }
                });
            }
        });

        let entries: Vec<(Vec<u8>, u64, Option<Vec<u8>>)> = l
            .iter_all()
            .map(|e| (e.key.to_vec(), e.version, e.value.map(|v| v.to_vec())))
            .collect();

        assert_eq!(entries.len(), THREADS * 1_000);
        assert_eq!(l.len(), entries.len());
        for w in entries.windows(2) {
            let ok = w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 > w[1].1);
            assert!(ok, "ordering broken at {:?} / {:?}", w[0], w[1]);
        }
        for (_, v, val) in &entries {
            match val {
                None => assert_eq!(v % 5, 0),
                Some(payload) => assert_eq!(payload, format!("{}", v).as_bytes()),
            }
        }

        // every write is visible at a snapshot at or above its version
        let mut newest: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for (k, v, _) in &entries {
            newest
                .entry(k.clone())
                .and_modify(|e| *e = (*e).max(*v))
                .or_insert(*v);
        }
        for (k, v) in newest {
            match l.get(MAX_VERSION, &k) {
                Some(e) => assert_eq!(e.version, v),
                None => panic!("key {:?} unreachable", k),
            }
        }
    }
}
