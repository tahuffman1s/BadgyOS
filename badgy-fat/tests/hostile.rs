//! The volume bytes are whatever a USB host chose to write, so every field in
//! them is attacker-controlled. These tests take a valid volume and corrupt one
//! thing at a time.
//!
//! The bar is not "parses correctly" -- most of these should be rejected. The
//! bar is that the parser returns a verdict: no panic, no unbounded loop, no
//! read outside the slice it was handed. On the badge a panic prints and spins
//! forever, so "refuses to mount" is a good outcome and "takes six seconds" is
//! not.

use std::time::{Duration, Instant};

use badgy_fat::{VOLUME_BYTES, Volume, format};

fn good() -> Vec<u8> {
    let mut v = vec![0u8; VOLUME_BYTES];
    format(&mut v, "BADGYOS", 0x1234_5678, &[("a.py", b"print(1)\n"), ("b.py", &[b'x'; 3000])]).unwrap();
    v
}

/// Parse and walk everything, insisting it finishes promptly.
fn probe(v: &[u8]) -> Option<usize> {
    let started = Instant::now();
    let n = match Volume::open(v) {
        Err(_) => None,
        Ok(vol) => {
            let files = vol.files();
            for f in &files {
                let _ = vol.read_file(f);
            }
            let _ = vol.label();
            let _ = vol.usage();
            Some(files.len())
        }
    };
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    n
}

fn put16(v: &mut [u8], off: usize, x: u16) { v[off..off + 2].copy_from_slice(&x.to_le_bytes()); }

fn put32(v: &mut [u8], off: usize, x: u32) { v[off..off + 4].copy_from_slice(&x.to_le_bytes()); }

#[test]
fn the_baseline_volume_parses() {
    assert_eq!(probe(&good()), Some(2));
}

#[test]
fn every_single_byte_of_the_boot_sector_can_be_corrupted() {
    // Exhaustive over the BPB: for each byte, try a handful of values. This is
    // the cheapest way to be sure no field is used before it is validated.
    let base = good();
    for off in 0..64usize {
        for val in [0x00u8, 0x01, 0x02, 0x7F, 0x80, 0xFF] {
            let mut v = base.clone();
            v[off] = val;
            probe(&v);
        }
    }
}

#[test]
fn pathological_geometry_is_rejected_rather_than_trusted() {
    // A name, and a mutation to apply to a known-good volume.
    type Case = (&'static str, Box<dyn Fn(&mut Vec<u8>)>);
    let cases: Vec<Case> = vec![
        ("sectors per cluster 0", Box::new(|v: &mut Vec<u8>| v[0x0D] = 0)),
        ("sectors per cluster 128", Box::new(|v: &mut Vec<u8>| v[0x0D] = 128)),
        ("sectors per cluster 3 (not a power of two)", Box::new(|v: &mut Vec<u8>| v[0x0D] = 3)),
        ("no FATs", Box::new(|v: &mut Vec<u8>| v[0x10] = 0)),
        ("sixteen FATs", Box::new(|v: &mut Vec<u8>| v[0x10] = 16)),
        ("no reserved sectors", Box::new(|v: &mut Vec<u8>| put16(v, 0x0E, 0))),
        ("huge reserved sectors", Box::new(|v: &mut Vec<u8>| put16(v, 0x0E, 0xFFFF))),
        ("no root entries", Box::new(|v: &mut Vec<u8>| put16(v, 0x11, 0))),
        ("root entries not a whole sector", Box::new(|v: &mut Vec<u8>| put16(v, 0x11, 3))),
        ("huge root entries", Box::new(|v: &mut Vec<u8>| put16(v, 0x11, 0xFFFF))),
        ("zero FAT sectors", Box::new(|v: &mut Vec<u8>| put16(v, 0x16, 0))),
        ("huge FAT sectors", Box::new(|v: &mut Vec<u8>| put16(v, 0x16, 0xFFFF))),
        ("sector size 4096", Box::new(|v: &mut Vec<u8>| put16(v, 0x0B, 4096))),
        ("sector size 0", Box::new(|v: &mut Vec<u8>| put16(v, 0x0B, 0))),
        ("no total sectors", Box::new(|v: &mut Vec<u8>| put16(v, 0x13, 0))),
        ("more sectors than the slice", Box::new(|v: &mut Vec<u8>| put16(v, 0x13, 0xFFFF))),
        (
            "32-bit total sectors at the limit",
            Box::new(|v: &mut Vec<u8>| {
                put16(v, 0x13, 0);
                put32(v, 0x20, u32::MAX);
            }),
        ),
        (
            "no boot signature",
            Box::new(|v: &mut Vec<u8>| {
                v[510] = 0;
                v[511] = 0;
            }),
        ),
    ];
    for (name, mutate) in cases {
        let mut v = good();
        mutate(&mut v);
        // Any verdict is fine; hanging or panicking is not. `probe` asserts both.
        let r = probe(&v);
        // A volume claiming more sectors than we hold must never be accepted:
        // every later index is derived from that number.
        if name.contains("more sectors") || name.contains("32-bit total sectors") {
            assert!(r.is_none(), "{} should have been rejected", name);
        }
    }
}

#[test]
fn a_directory_full_of_garbage_yields_a_verdict() {
    let base = good();
    // Several unrelated fill patterns, since a single one might miss a state.
    for seed in [0x00u8, 0x2E, 0x41, 0xE5, 0x0F, 0xFF] {
        let mut v = base.clone();
        for b in v[7 * 512..11 * 512].iter_mut() {
            *b = seed;
        }
        probe(&v);
    }
    // ...and pseudo-random bytes, which is what actually finds state-machine holes.
    let mut state: u32 = 0x9e37_79b9;
    for _ in 0..200 {
        let mut v = base.clone();
        for b in v[7 * 512..11 * 512].iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        probe(&v);
    }
}

#[test]
fn a_long_name_entry_with_no_short_entry_after_it_is_harmless() {
    let mut v = good();
    let root = 7 * 512;
    // A directory made entirely of long-name fragments: the assembly buffer must
    // not grow without bound and must never be used without a short entry to
    // check it against.
    for i in 0..60 {
        let e = root + i * 32;
        v[e] = 0x01; // sequence 1, never the last of a set
        v[e + 11] = 0x0F; // long-name attribute
        v[e + 13] = 0x42; // some checksum
    }
    assert_eq!(probe(&v), Some(0));
}

#[test]
fn a_directory_entry_claiming_a_gigabyte_does_not_walk_forever() {
    let mut v = good();
    let root = 7 * 512;
    // Entry 1 is the first seeded file. Leave its chain alone and just lie about
    // the size: the walk is bounded by the cluster count, not by the claim.
    put32(&mut v, root + 32 + 28, 0xFFFF_FFFF);
    // It must be rejected -- no file on a 512 KiB volume is 4 GB -- and quickly.
    let files = probe(&v).expect("volume should still parse");
    assert_eq!(files, 1, "the oversized entry should have been skipped");
}

#[test]
fn a_fat_full_of_end_markers_or_cycles_terminates() {
    let base = good();
    for fill in [0xFFu8, 0x00, 0x22, 0xAA] {
        let mut v = base.clone();
        for b in v[512..4 * 512].iter_mut() {
            *b = fill;
        }
        probe(&v);
    }
}

#[test]
fn truncated_slices_are_refused() {
    let v = good();
    for len in [0usize, 1, 16, 511, 512, 513, VOLUME_BYTES - 1] {
        assert!(Volume::open(&v[..len]).is_err(), "a {}-byte slice should not parse", len);
    }
}
