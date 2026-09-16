//! Append-only lock-free memory arena allocator

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::atomic::{AtomicU32, Ordering};

// offset 0 is reserved, it can mean null as no value
pub const NULL: u32 = 0;

// reserved prefix, keeps offset 0 unusable and gives the head node 8 byte alignment for free
const RESERVED: u32 = 8;

pub struct Arena {
    ptr: *mut u8,
    layout: Layout,
    cap: u32,
    // bump cursor, the only mutable state and this is where writers contend on
    len: AtomicU32,
}

// every mutation goes through len (atomic CAS) or writes to a region
// this thread exclusively owns because it just claimed that range from len.
// Two threads can never receive overlapping ranges from alloc
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaFull;

impl Arena {
    // the memtable needs dictate to have the cap limited, as we need
    // to flush it on the disk once it's full. Generally, we try to avoid growing a
    // lock-free arena, as this will require the mem reclamation
    pub fn new(cap: u32) -> Self {
        assert!(cap > RESERVED, "arena capacity too small");

        let layout = Layout::from_size_align(cap as usize, 8).expect("bad arena layout");
        // zeroed so skiplist tower slots read as null before init
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "arena allocation failed");

        Self {
            ptr: ptr,
            layout: layout,
            cap,
            len: AtomicU32::new(RESERVED),
        }
    }

    pub fn alloc(&self, size: u32, align: u32) -> Result<u32, ArenaFull> {
        debug_assert!(align.is_power_of_two());
        let mut cur = self.len.load(Ordering::Relaxed);
        loop {
            let start = (cur.checked_add(align - 1).ok_or(ArenaFull)?) & !(align - 1);
            let end = start.checked_add(size).ok_or(ArenaFull)?;
            if end > self.cap {
                return Err(ArenaFull);
            }

            match self
                .len
                .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(start),
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn alloc_bytes(&self, data: &[u8]) -> Result<u32, ArenaFull> {
        let off = self.alloc(data.len() as u32, 1)?;
        if !data.is_empty() {
            unsafe {
                // off..off+len was just claimed exclusively by this thread
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.ptr.add(off as usize),
                    data.len(),
                );
            }
        }
        Ok(off)
    }

    // offset must be within the range previously returned by alloc
    #[inline]
    pub unsafe fn ptr_at(&self, offset: u32) -> *mut u8 {
        debug_assert!(offset < self.cap);
        self.ptr.add(offset as usize)
    }

    #[inline]
    pub unsafe fn slice(&self, offset: u32, len: u32) -> &[u8] {
        if len == 0 {
            return &[];
        }
        std::slice::from_raw_parts(self.ptr.add(offset as usize), len as usize)
    }

    #[inline]
    pub fn allocated(&self) -> usize {
        self.len.load(Ordering::Acquire) as usize
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.capacity() - self.allocated()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr, self.layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Barrier;
    use std::thread;

    const CAP: u32 = 1 << 20;

    fn arena() -> Arena {
        Arena::new(CAP)
    }

    /// Every alloc must satisfy all of these, always.
    fn check_claim(a: &Arena, off: u32, size: u32, align: u32) {
        assert_eq!(off % align, 0, "offset {} not {}-aligned", off, align);
        assert!(
            off >= RESERVED,
            "offset {} is inside the reserved prefix",
            off
        );
        assert_ne!(off, NULL, "alloc handed out the null offset");
        assert!(
            off as u64 + size as u64 <= a.capacity() as u64,
            "claim {}..{} runs past capacity {}",
            off,
            off + size,
            a.capacity()
        );
        // the returned offset must be addressable
        unsafe {
            let p = a.ptr_at(off);
            assert_eq!(
                p as usize % align as usize,
                0,
                "offset {} is {}-aligned but the pointer is not; the base allocation is under-aligned",
                off,
                align
            );
        }
    }

    // ---------------------------------------------------------------- construction

    #[test]
    fn new_reserves_the_prefix() {
        let a = arena();
        assert_eq!(a.capacity(), CAP as usize);
        assert_eq!(a.allocated(), RESERVED as usize);
        assert_eq!(a.remaining(), (CAP - RESERVED) as usize);
    }

    #[test]
    #[should_panic(expected = "arena capacity too small")]
    fn new_rejects_a_capacity_inside_the_prefix() {
        Arena::new(RESERVED);
    }

    #[test]
    fn smallest_usable_arena() {
        let a = Arena::new(RESERVED + 1);
        let off = a.alloc(1, 1).unwrap();
        check_claim(&a, off, 1, 1);
        assert_eq!(a.remaining(), 0);
        assert_eq!(a.alloc(1, 1), Err(ArenaFull));
    }

    #[test]
    fn memory_starts_zeroed() {
        // the skiplist relies on this: tower slots must read as NULL before init
        let a = arena();
        let off = a.alloc(4096, 8).unwrap();
        unsafe {
            let s = a.slice(off, 4096);
            assert!(s.iter().all(|b| *b == 0), "arena handed out dirty memory");
        }
    }

    // ---------------------------------------------------------------- alignment

    #[test]
    fn every_alignment_is_honoured() {
        let a = arena();
        // sizes chosen to leave the cursor at awkward values between calls
        for align in [1u32, 2, 4, 8] {
            for size in [1u32, 3, 5, 7, 9, 17, 31] {
                let off = a.alloc(size, align).unwrap();
                check_claim(&a, off, size, align);
            }
        }
    }

    #[test]
    fn alignment_holds_after_unaligned_byte_runs() {
        // this is the exact sequence the skiplist's put() performs:
        // key bytes at align 1, value bytes at align 1, then a node at align 8
        let a = arena();
        for len in 1u32..64 {
            let key = a.alloc(len, 1).unwrap();
            let value = a.alloc(len, 1).unwrap();
            let node = a.alloc(24 + 4 * 12, 8).unwrap();
            check_claim(&a, key, len, 1);
            check_claim(&a, value, len, 1);
            check_claim(&a, node, 24 + 4 * 12, 8);
        }
    }

    #[test]
    fn alignment_padding_is_not_handed_out_twice() {
        let a = arena();
        let one = a.alloc(1, 1).unwrap();
        let eight = a.alloc(8, 8).unwrap();
        assert!(
            eight >= one + 1,
            "aligned alloc at {} overlaps the byte at {}",
            eight,
            one
        );
    }

    // ---------------------------------------------------------------- bump discipline

    #[test]
    fn the_cursor_only_moves_forward() {
        let a = arena();
        let mut prev_end = RESERVED;
        let mut rng: u32 = 12345;
        for _ in 0..2000 {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let size = (rng >> 16) % 64 + 1;
            let align = 1u32 << ((rng >> 8) % 4);
            let before = a.allocated() as u32;
            let off = a.alloc(size, align).unwrap();
            check_claim(&a, off, size, align);
            assert!(
                off >= prev_end,
                "alloc returned {} which overlaps the previous claim ending at {}",
                off,
                prev_end
            );
            assert!(
                a.allocated() as u32 >= before,
                "the bump cursor moved backwards: {} -> {}",
                before,
                a.allocated()
            );
            prev_end = off + size;
        }
    }

    #[test]
    fn claims_never_overlap() {
        let a = arena();
        let mut claims: Vec<(u32, u32)> = Vec::new();
        for i in 0..500u32 {
            let size = i % 37 + 1;
            let align = 1u32 << (i % 4);
            let off = a.alloc(size, align).unwrap();
            claims.push((off, off + size));
        }
        claims.sort();
        for w in claims.windows(2) {
            assert!(w[0].1 <= w[1].0, "claims {:?} and {:?} overlap", w[0], w[1]);
        }
    }

    #[test]
    fn distinct_allocations_have_distinct_offsets() {
        let a = arena();
        let offs: BTreeSet<u32> = (0..1000).map(|_| a.alloc(8, 8).unwrap()).collect();
        assert_eq!(offs.len(), 1000, "alloc returned a duplicate offset");
    }

    #[test]
    fn accounting_invariant_holds_throughout() {
        let a = arena();
        for i in 0..500u32 {
            a.alloc(i % 31 + 1, 8).unwrap();
            assert_eq!(
                a.allocated() + a.remaining(),
                a.capacity(),
                "allocated + remaining drifted from capacity"
            );
        }
    }

    // ---------------------------------------------------------------- byte copies

    #[test]
    fn alloc_bytes_round_trips() {
        let a = arena();
        let payloads: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("payload-{}-{}", i, "x".repeat(i % 50)).into_bytes())
            .collect();
        let offs: Vec<u32> = payloads.iter().map(|p| a.alloc_bytes(p).unwrap()).collect();
        for (off, want) in offs.iter().zip(&payloads) {
            unsafe {
                assert_eq!(a.slice(*off, want.len() as u32), &want[..]);
            }
        }
    }

    #[test]
    fn alloc_bytes_does_not_disturb_its_neighbours() {
        let a = arena();
        let a_off = a.alloc_bytes(b"aaaa").unwrap();
        let b_off = a.alloc_bytes(b"bbbbbbbb").unwrap();
        let c_off = a.alloc_bytes(b"cc").unwrap();
        unsafe {
            assert_eq!(a.slice(a_off, 4), b"aaaa");
            assert_eq!(a.slice(b_off, 8), b"bbbbbbbb");
            assert_eq!(a.slice(c_off, 2), b"cc");
        }
    }

    #[test]
    fn empty_slices_are_handled() {
        let a = arena();
        let off = a.alloc_bytes(b"").unwrap();
        assert_ne!(off, NULL, "an empty alloc collided with the null offset");
        unsafe {
            assert_eq!(a.slice(off, 0), b"");
        }
        // a zero-length read anywhere is empty, and must not touch memory
        unsafe {
            assert_eq!(a.slice(0, 0), b"");
        }
        // the arena is still usable afterwards
        let next = a.alloc_bytes(b"after").unwrap();
        unsafe {
            assert_eq!(a.slice(next, 5), b"after");
        }
    }

    #[test]
    fn byte_content_survives_later_allocations() {
        let a = arena();
        let first = a.alloc_bytes(b"do not move me").unwrap();
        for i in 0..1000u32 {
            a.alloc(i % 17 + 1, 8).unwrap();
        }
        unsafe {
            assert_eq!(a.slice(first, 14), b"do not move me");
        }
    }

    // ---------------------------------------------------------------- exhaustion

    #[test]
    fn exhaustion_reports_full() {
        let a = Arena::new(1024);
        let mut total = 0u32;
        loop {
            match a.alloc(64, 8) {
                Ok(off) => {
                    check_claim(&a, off, 64, 8);
                    total += 64;
                }
                Err(e) => {
                    assert_eq!(e, ArenaFull);
                    break;
                }
            }
            assert!(total < 1024, "arena handed out more than its capacity");
        }
    }

    #[test]
    fn a_failed_alloc_does_not_consume_anything() {
        let a = Arena::new(1024);
        a.alloc(900, 8).unwrap();
        let before = a.allocated();
        assert_eq!(a.alloc(500, 8), Err(ArenaFull));
        assert_eq!(
            a.allocated(),
            before,
            "a rejected alloc still advanced the cursor"
        );
        // a request that does fit must still succeed after the rejection
        let off = a.alloc(16, 8).unwrap();
        check_claim(&a, off, 16, 8);
    }

    #[test]
    fn oversized_requests_are_rejected_not_wrapped() {
        let a = Arena::new(1024);
        assert_eq!(a.alloc(u32::MAX, 1), Err(ArenaFull));
        assert_eq!(a.alloc(u32::MAX, 8), Err(ArenaFull));
        assert_eq!(a.alloc(u32::MAX - 4, 8), Err(ArenaFull));
        // and the arena is unharmed
        assert_eq!(a.allocated(), RESERVED as usize);
        let off = a.alloc(8, 8).unwrap();
        check_claim(&a, off, 8, 8);
    }

    #[test]
    fn a_request_that_exactly_fills_the_arena_succeeds() {
        let a = Arena::new(1024);
        let off = a.alloc(1024 - RESERVED, 8).unwrap();
        check_claim(&a, off, 1024 - RESERVED, 8);
        assert_eq!(a.remaining(), 0);
        assert_eq!(a.alloc(1, 1), Err(ArenaFull));
    }

    #[test]
    fn alloc_bytes_reports_full_rather_than_copying() {
        let a = Arena::new(1024);
        a.alloc(1000, 8).unwrap();
        assert_eq!(a.alloc_bytes(&[7u8; 256]), Err(ArenaFull));
        assert_eq!(a.remaining(), (1024 - 1008) as usize);
    }

    // ---------------------------------------------------------------- concurrency

    #[test]
    fn concurrent_allocs_never_overlap() {
        const THREADS: usize = 8;
        const PER_THREAD: u32 = 2_000;
        let a = Arena::new(1 << 22);
        let barrier = Barrier::new(THREADS);

        let claims: Vec<Vec<(u32, u32, u8)>> = thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let a = &a;
                    let barrier = &barrier;
                    s.spawn(move || {
                        let mut mine = Vec::with_capacity(PER_THREAD as usize);
                        barrier.wait();
                        for i in 0..PER_THREAD {
                            let size = (i % 24) + 1;
                            let align = 1u32 << (i % 4);
                            let off = a.alloc(size, align).unwrap();
                            assert_eq!(off % align, 0, "unaligned offset under contention");
                            // stamp the claimed range with this thread's id
                            unsafe {
                                let p = a.ptr_at(off);
                                for b in 0..size {
                                    p.add(b as usize).write(t as u8 + 1);
                                }
                            }
                            mine.push((off, size, t as u8 + 1));
                        }
                        mine
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // nobody's bytes were overwritten by anybody else
        let mut all: Vec<(u32, u32, u8)> = claims.into_iter().flatten().collect();
        assert_eq!(all.len(), THREADS * PER_THREAD as usize);
        for (off, size, stamp) in &all {
            unsafe {
                let s = a.slice(*off, *size);
                assert!(
                    s.iter().all(|b| b == stamp),
                    "range {}..{} claimed by thread {} was written by another thread: {:?}",
                    off,
                    off + size,
                    stamp,
                    s
                );
            }
        }

        // and no two claims overlapped in the first place
        all.sort();
        for w in all.windows(2) {
            assert!(
                w[0].0 + w[0].1 <= w[1].0,
                "threads {} and {} were given overlapping ranges {:?} / {:?}",
                w[0].2,
                w[1].2,
                w[0],
                w[1]
            );
        }

        let claimed: u32 = all.iter().map(|(_, size, _)| size).sum();
        assert!(
            a.allocated() as u32 >= claimed + RESERVED,
            "the cursor accounts for less than was handed out"
        );
    }

    #[test]
    fn concurrent_alloc_bytes_round_trips() {
        const THREADS: usize = 8;
        let a = Arena::new(1 << 22);
        let barrier = Barrier::new(THREADS);

        thread::scope(|s| {
            for t in 0..THREADS {
                let a = &a;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    let mut mine = Vec::new();
                    for i in 0..2_000 {
                        let payload = format!("{}:{}:{}", t, i, "z".repeat(i % 40)).into_bytes();
                        let off = a.alloc_bytes(&payload).unwrap();
                        mine.push((off, payload));
                    }
                    for (off, want) in mine {
                        unsafe {
                            assert_eq!(
                                a.slice(off, want.len() as u32),
                                &want[..],
                                "payload at {} was corrupted by a concurrent writer",
                                off
                            );
                        }
                    }
                });
            }
        });
    }

    #[test]
    fn concurrent_exhaustion_stays_within_capacity() {
        const THREADS: usize = 8;
        const CAPACITY: u32 = 64 * 1024;
        let a = Arena::new(CAPACITY);
        let barrier = Barrier::new(THREADS);

        let claims: Vec<Vec<(u32, u32)>> = thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let a = &a;
                    let barrier = &barrier;
                    s.spawn(move || {
                        let mut mine = Vec::new();
                        barrier.wait();
                        // every thread races to the end of the arena
                        while let Ok(off) = a.alloc(48, 8) {
                            assert!(
                                off as u64 + 48 <= CAPACITY as u64,
                                "claim {}..{} escapes the arena",
                                off,
                                off + 48
                            );
                            unsafe {
                                a.ptr_at(off).write(1);
                            }
                            mine.push((off, 48u32));
                        }
                        mine
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut all: Vec<(u32, u32)> = claims.into_iter().flatten().collect();
        all.sort();
        for w in all.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "overlap at the arena tail");
        }
        assert!(a.allocated() <= a.capacity());
        assert_eq!(a.allocated() + a.remaining(), a.capacity());
        // one more request must keep failing, not suddenly succeed
        for _ in 0..100 {
            assert_eq!(a.alloc(48, 8), Err(ArenaFull));
        }
    }
}
