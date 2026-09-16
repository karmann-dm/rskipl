# rskipl

[![CI](https://github.com/karmann-dm/rskipl/actions/workflows/build-pr.yaml/badge.svg)](https://github.com/karmann-dm/rskipl/actions/workflows/build-pr.yaml)
[![crates.io](https://img.shields.io/crates/v/rskipl.svg)](https://crates.io/crates/rskipl)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A lock-free, multi-version (MVCC) skiplist for Rust, built on a single pre-allocated memory arena. Zero dependencies.

It is designed to be the **memtable** of an LSM-tree storage engine cause many threads write into it at once, readers look up keys "as of" a snapshot version, and once it fills up you freeze it, flush it to disk and throw the whole thing away.

> **Status:** early (0.1). The core data structure is implemented and well tested

## Why?

A typical LSM engine keeps recent writes in an in-memory sorted structure before flushing them to disk. That structure needs to:

- accept writes from many threads without a global lock,
- keep several versions of the same key so readers can see a consistent snapshot,
- have a hard size limit, so you know when to flush,
- be cheap to throw away.

rskipl does exactly that. All nodes, keys and values live in one contiguous block of memory. Allocation is a single atomic "bump" of a cursor, nothing is ever freed individually, and dropping the list releases everything in one go. Because nothing is freed while the list is alive, there is no need for epoch-based reclamation or hazard pointers.

## Features

- **Lock-free** inserts and reads, using only atomics and compare-and-swap
- **MVCC**: every entry is a `(key, version)` pair; reads take a snapshot version
- **Tombstones**: deletes are recorded as entries, so they correctly hide older versions
- **Fixed capacity**: the arena never grows; a full list returns `Error::Full` instead of panicking
- **Zero dependencies**, no `unsafe` required from the caller
- Byte-string keys and values (`&[u8]`), ordered bytewise

## Installation

```sh
cargo add rskipl
```

Requires Rust 1.85 or newer (edition 2024).

## Quick start

```rust
use rskipl::SkipList;

fn main() -> Result<(), rskipl::Error> {
    // A skiplist backed by a 64 MiB arena
    let list = SkipList::new(64 * 1024 * 1024)?;

    list.insert(1, b"apple", b"red")?;
    list.insert(2, b"apple", b"green")?;
    list.remove(3, b"apple")?; // tombstone at version 3

    // Each read sees the newest version at or below its snapshot
    assert!(list.get(0, b"apple").is_none());                        // nothing yet
    assert_eq!(list.get(1, b"apple").unwrap().value, Some(&b"red"[..]));
    assert_eq!(list.get(2, b"apple").unwrap().value, Some(&b"green"[..]));
    assert_eq!(list.get(3, b"apple").unwrap().value, None);          // deleted

    Ok(())
}
```

### Writing from many threads

`SkipList` is `Send + Sync`, and all methods take `&self`, so you can share it directly:

```rust
use rskipl::SkipList;
use std::thread;

let list = SkipList::new(64 * 1024 * 1024).unwrap();

thread::scope(|s| {
    for t in 0..8u64 {
        let list = &list;
        s.spawn(move || {
            for i in 0..1_000u64 {
                let key = format!("t{t}-k{i}");
                list.insert(i, key.as_bytes(), b"value").unwrap();
            }
        });
    }
});

assert_eq!(list.len(), 8_000);
```

### Handling a full memtable

```rust
use rskipl::{Error, SkipList};

match memtable.insert(version, key, value) {
    Ok(()) => {}
    Err(Error::Full) => {
        // Freeze this memtable, schedule a flush,
        // start a fresh SkipList and retry the write there.
    }
    Err(Error::TooLarge) => {
        // The key or value does not fit in a u32 length.
    }
}
```

### Iterating

`iter_all()` walks every entry, including old versions and tombstones, in storage order. This is what a flush to disk needs.

```rust
for entry in list.iter_all() {
    match entry.value {
        Some(v) => println!("{:?} @ {} = {:?}", entry.key, entry.version, v),
        None    => println!("{:?} @ {} = <deleted>", entry.key, entry.version),
    }
}
```

## API at a glance

| Method | What it does |
| --- | --- |
| `SkipList::new(capacity)` | Create a list with a fixed arena size in bytes (max ~4 GiB) |
| `insert(version, key, value)` | Write a value for `key` at `version` |
| `remove(version, key)` | Write a tombstone for `key` at `version` |
| `get(snapshot, key)` | Newest entry for `key` with `version <= snapshot`, if any |
| `iter_all()` | Every entry, ordered by key ascending, version descending |
| `len()` / `is_empty()` | Number of distinct `(key, version)` entries |
| `allocated()` / `remaining()` / `capacity()` | Arena usage in bytes |

An `Entry` has three fields: `key`, `version`, and `value`, where `value: None` means the key was deleted at that version. That is different from `get` returning `None`, which means no version of the key is visible at all. An LSM read path should stop searching older levels when it sees a tombstone.

## Semantics

- **Ordering.** Entries are sorted by key (bytewise, ascending), then by version (descending). The newest version of a key always comes first.
- **Snapshots.** `get(s, k)` returns the entry for `k` with the largest version that is `<= s`.
- **Versions** range from `0` to `MAX_VERSION` (2⁵⁶ − 1). Assigning versions is up to you, typically a global sequence number.
- **Same key, same version.** Writing a `(key, version)` pair that already exists replaces its value in place. This is atomic: readers see either the old or the new value, never a mix.
- **Empty values are not deletes.** `insert(v, k, b"")` stores an empty value; only `remove` creates a tombstone.

## How it works

### The arena

The arena is one zeroed, 8-byte-aligned allocation. The only mutable state is an `AtomicU32` cursor. To allocate, a thread reads the cursor, computes the aligned range it wants, and tries to CAS the cursor forward. If it wins, that range is exclusively its own; if it loses, it retries with the new cursor value. Two threads can never receive overlapping ranges.

Everything inside the arena is addressed by a `u32` offset rather than a pointer. Offset `0` is reserved and means "null". Using 32-bit offsets keeps nodes small, and is why a single list is limited to about 4 GiB.

### Nodes

Each node is laid out like this:

| Offset | Field | Notes |
| --- | --- | --- |
| +0 | `version: u64` | Immutable once linked |
| +8 | `value: AtomicU64` | Upper 32 bits: offset, lower 32 bits: length. Length `u32::MAX` marks a tombstone |
| +16 | `key_off: u32` | Immutable once linked |
| +20 | `key_len: u32` | Immutable once linked |
| +24 | `tower: [AtomicU32; height]` | Forward links, only `height` slots are allocated |

Packing the value's offset and length into one atomic word is what makes in-place overwrites safe for concurrent readers.

Node heights are random with a branching factor of 1/4 and a maximum of 20, which comfortably covers any realistic memtable size. The random generator is a lock-free SplitMix64 counter, so no thread-local state or `rand` crate is needed.

### Inserting

1. Walk down from the top level, recording the neighbours (`prev`, `next`) of the new position at every level. If an exact `(key, version)` match is found, overwrite its value and stop.
2. Copy the key and value into the arena, then allocate and fully initialise the node.
3. Link the node in **from the bottom level up**, one CAS per level. If a CAS fails because another thread inserted nearby, only that level's neighbours are recomputed and the CAS is retried.

Because a node is fully written before it is linked, and level 0 is linked first, readers never observe a half-built node, and any node reachable from an upper level is already reachable on level 0.

## Limitations

- **No reclamation.** Memory is only released when the whole list is dropped. Overwriting the same `(key, version)` leaves the old value's bytes in the arena.
- **Fixed size, up to ~4 GiB** per list.
- **No snapshot-aware range scans yet.** Reads are point lookups (`get`) or a full walk (`iter_all`).
- **`MAX_VERSION` is only checked in debug builds.**

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The test suite covers the arena (alignment, exhaustion, concurrent allocation), MVCC and tombstone behaviour, a randomized comparison against a `BTreeMap` model, and several multi-threaded stress tests, including one that checks readers never see a partially linked node.

Releases are automated with [release-plz](https://release-plz.dev); see [CHANGELOG.md](CHANGELOG.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
