//! Laying down a blank FAT12 volume, plus seeding it with a few files.
//!
//! This runs once, the first time the badge finds no valid volume in storage.
//! After that the host owns the filesystem and this code never touches it
//! again -- which is deliberate: two writers is how filesystems get corrupted.
//!
//! Seeding matters more than it sounds. A drive that mounts empty tells the
//! user nothing; a drive with a `README.txt` and a working `hello.py` on it
//! explains itself.

use crate::geom::*;
use crate::{ATTR_ARCHIVE, ATTR_VOLUME_ID};

/// Size of the volume this module produces.
pub const VOLUME_BYTES: usize = TOTAL_SECTORS * BYTES_PER_SECTOR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// The destination is smaller than [`VOLUME_BYTES`].
    TooSmall,
    /// A seed file's name does not fit 8.3, or its content does not fit.
    BadSeed,
    /// Ran out of clusters or root directory entries while seeding.
    Full,
}

/// Write a blank volume into `buf`, then add `seed` files to it.
///
/// `serial` becomes the volume ID. It should differ between badges: Windows
/// uses it (together with the label) to tell two removable volumes apart, and
/// two identical ones plugged into the same machine will not both get a drive
/// letter.
pub fn format(buf: &mut [u8], label: &str, serial: u32, seed: &[(&str, &[u8])]) -> Result<(), FormatError> {
    if buf.len() < VOLUME_BYTES {
        return Err(FormatError::TooSmall);
    }
    let vol = &mut buf[..VOLUME_BYTES];
    vol.fill(0);

    write_boot_sector(vol, label, serial);

    // FAT[0] carries the media descriptor in its low byte and is otherwise all
    // ones; FAT[1] is the end-of-chain marker. Both are convention rather than
    // anything the driver reads, but tools check them.
    for fat in 0..NUM_FATS {
        let base = (FAT_START + fat * FAT_SECTORS) * BYTES_PER_SECTOR;
        let fat = &mut vol[base..base + FAT_SECTORS * BYTES_PER_SECTOR];
        set_fat12(fat, 0, 0xFF8);
        set_fat12(fat, 1, 0xFFF);
    }

    // The label the host displays comes from a root directory entry, not from
    // the boot sector copy. Both get written; only this one is shown.
    let name = pad11(label);
    let entry = &mut vol[ROOT_START * BYTES_PER_SECTOR..ROOT_START * BYTES_PER_SECTOR + 32];
    entry[..11].copy_from_slice(&name);
    entry[11] = ATTR_VOLUME_ID;

    let mut next_cluster = 2u32;
    let mut next_entry = 1u32;
    for (name, content) in seed {
        add_file(vol, name, content, &mut next_cluster, &mut next_entry)?;
    }
    Ok(())
}

fn write_boot_sector(vol: &mut [u8], label: &str, serial: u32) {
    // A jump instruction has to be here; some hosts sanity-check it before
    // they will look at anything else in the sector.
    vol[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    vol[3..11].copy_from_slice(b"BADGYOS ");

    let put16 = |v: &mut [u8], off: usize, x: u16| v[off..off + 2].copy_from_slice(&x.to_le_bytes());
    let put32 = |v: &mut [u8], off: usize, x: u32| v[off..off + 4].copy_from_slice(&x.to_le_bytes());

    put16(vol, 0x0B, BYTES_PER_SECTOR as u16);
    vol[0x0D] = SECTORS_PER_CLUSTER as u8;
    put16(vol, 0x0E, RESERVED_SECTORS as u16);
    vol[0x10] = NUM_FATS as u8;
    put16(vol, 0x11, ROOT_ENTRIES as u16);
    put16(vol, 0x13, TOTAL_SECTORS as u16);
    vol[0x15] = 0xF8; // fixed disk; must match FAT[0]'s low byte
    put16(vol, 0x16, FAT_SECTORS as u16);
    // Cylinder/head geometry is meaningless for this device but should not be
    // zero -- a few tools divide by it.
    put16(vol, 0x18, 16);
    put16(vol, 0x1A, 2);
    put32(vol, 0x1C, 0); // hidden sectors: 0, this volume is not in a partition
    put32(vol, 0x20, 0); // total sectors (32-bit): unused, the 16-bit field is set

    vol[0x24] = 0x80; // drive number
    vol[0x25] = 0;
    vol[0x26] = 0x29; // extended boot signature: the next three fields are valid
    put32(vol, 0x27, serial);
    vol[0x2B..0x36].copy_from_slice(&pad11(label));
    vol[0x36..0x3E].copy_from_slice(b"FAT12   ");

    vol[510] = 0x55;
    vol[511] = 0xAA;
}

/// Append a file, allocating clusters consecutively from `next_cluster`.
///
/// Only used for seeding, so it can assume the volume is empty ahead of it and
/// skip free-space search entirely.
fn add_file(
    vol: &mut [u8],
    name: &str,
    content: &[u8],
    next_cluster: &mut u32,
    next_entry: &mut u32,
) -> Result<(), FormatError> {
    let (sfn, nt_flags) = short_name_of(name).ok_or(FormatError::BadSeed)?;
    if *next_entry >= ROOT_ENTRIES as u32 {
        return Err(FormatError::Full);
    }

    let bpc = (BYTES_PER_SECTOR * SECTORS_PER_CLUSTER) as u32;
    let clusters = (content.len() as u32).div_ceil(bpc).max(1);
    if *next_cluster + clusters > CLUSTER_COUNT as u32 + 2 {
        return Err(FormatError::Full);
    }
    let first = *next_cluster;

    for i in 0..clusters {
        let cluster = first + i;
        let off = (DATA_START + (cluster as usize - 2) * SECTORS_PER_CLUSTER) * BYTES_PER_SECTOR;
        let start = (i * bpc) as usize;
        let take = content.len().saturating_sub(start).min(bpc as usize);
        vol[off..off + take].copy_from_slice(&content[start..start + take]);

        let link = if i + 1 == clusters { 0xFFF } else { cluster + 1 };
        // Both FATs are kept identical; a host that reads the second one and
        // finds it stale will call the volume damaged.
        for fat in 0..NUM_FATS {
            let base = (FAT_START + fat * FAT_SECTORS) * BYTES_PER_SECTOR;
            set_fat12(&mut vol[base..base + FAT_SECTORS * BYTES_PER_SECTOR], cluster, link);
        }
    }

    let eoff = ROOT_START * BYTES_PER_SECTOR + *next_entry as usize * 32;
    let e = &mut vol[eoff..eoff + 32];
    e[..11].copy_from_slice(&sfn);
    e[11] = ATTR_ARCHIVE;
    e[12] = nt_flags;
    // No timestamps: leaving them zero is legal, and there is no clock to read.
    e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    e[28..32].copy_from_slice(&(content.len() as u32).to_le_bytes());

    *next_cluster += clusters;
    *next_entry += 1;
    Ok(())
}

/// Convert `name` to a padded 8.3 short name plus the case flags that let a
/// host display it in lower case.
///
/// Returns `None` for anything that will not fit, because generating a `~1`
/// tail (and checking it for collisions) is real work that seeding does not
/// need -- the seed filenames are ours to choose.
fn short_name_of(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    if !base.is_ascii() || !ext.is_ascii() {
        return None;
    }
    // Mixed case cannot be represented: the flags apply to the whole base or
    // the whole extension.
    let base_lower = base.chars().all(|c| !c.is_ascii_uppercase());
    let ext_lower = ext.chars().all(|c| !c.is_ascii_uppercase());
    if !(base_lower || base.chars().all(|c| !c.is_ascii_lowercase())) {
        return None;
    }
    if !(ext_lower || ext.chars().all(|c| !c.is_ascii_lowercase())) {
        return None;
    }
    for c in name.chars() {
        if c != '.' && !is_sfn_char(c as u8) {
            return None;
        }
    }

    let mut out = [b' '; 11];
    for (i, c) in base.bytes().enumerate() {
        out[i] = c.to_ascii_uppercase();
    }
    for (i, c) in ext.bytes().enumerate() {
        out[8 + i] = c.to_ascii_uppercase();
    }
    let mut flags = 0u8;
    if base_lower && base.chars().any(|c| c.is_ascii_lowercase()) {
        flags |= 0x08;
    }
    if ext_lower && ext.chars().any(|c| c.is_ascii_lowercase()) {
        flags |= 0x10;
    }
    Some((out, flags))
}

fn is_sfn_char(c: u8) -> bool { c.is_ascii_alphanumeric() || b"$%'-_@~`!(){}^#&".contains(&c) }

fn pad11(s: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (i, c) in s.bytes().take(11).enumerate() {
        out[i] = c.to_ascii_uppercase();
    }
    out
}

/// Write a 12-bit FAT entry. Neighbouring entries share a byte, so the odd and
/// even cases have to preserve the other half.
fn set_fat12(fat: &mut [u8], cluster: u32, value: u32) {
    let k = (cluster + cluster / 2) as usize;
    if k + 1 >= fat.len() {
        return;
    }
    if cluster & 1 == 0 {
        fat[k] = (value & 0xFF) as u8;
        fat[k + 1] = (fat[k + 1] & 0xF0) | ((value >> 8) & 0x0F) as u8;
    } else {
        fat[k] = (fat[k] & 0x0F) | (((value & 0x0F) as u8) << 4);
        fat[k + 1] = ((value >> 4) & 0xFF) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat12_entries_round_trip_including_shared_bytes() {
        let mut fat = [0u8; 64];
        for (n, v) in [(0u32, 0xFF8u32), (1, 0xFFF), (2, 3), (3, 0xFFF), (10, 0x123), (11, 0xABC)] {
            set_fat12(&mut fat, n, v);
        }
        // Read them back the way `read.rs` does.
        let get = |n: u32| {
            let k = (n + n / 2) as usize;
            let (lo, hi) = (fat[k] as u32, fat[k + 1] as u32);
            if n & 1 == 0 { ((hi & 0x0F) << 8) | lo } else { (hi << 4) | (lo >> 4) }
        };
        assert_eq!(get(0), 0xFF8);
        assert_eq!(get(1), 0xFFF);
        assert_eq!(get(2), 3);
        assert_eq!(get(3), 0xFFF);
        assert_eq!(get(10), 0x123);
        assert_eq!(get(11), 0xABC);
    }

    #[test]
    fn short_names_and_case_flags() {
        assert_eq!(short_name_of("hello.py").unwrap(), (*b"HELLO   PY ", 0x18));
        assert_eq!(short_name_of("README.TXT").unwrap(), (*b"README  TXT", 0x00));
        assert_eq!(short_name_of("NoGood.py"), None, "mixed case cannot be flagged");
        assert_eq!(short_name_of("waytoolongname.py"), None);
        assert_eq!(short_name_of("has space.py"), None);
    }
}
