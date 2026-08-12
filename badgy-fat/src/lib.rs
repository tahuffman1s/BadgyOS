//! A read-only FAT12/FAT16 reader, plus a formatter for a blank volume.
//!
//! # Why only read
//!
//! The badge presents its script volume over USB mass storage, so the *host*
//! does all the writing: it allocates clusters, writes directory entries and
//! generates short names. The badge only ever has to answer "what files are on
//! here, and what is in them" -- which is a fraction of a full FAT
//! implementation and, more to the point, a fraction of the risk. The one thing
//! written here is [`format`], which lays down a blank volume the first time.
//!
//! # The part that is not obvious
//!
//! A host does not write a file atomically. Watching a real `cp` onto a FAT12
//! volume, the directory entry appears *first* with size 0, then again with an
//! intermediate size and a valid-looking cluster chain, and only then with the
//! real size. So "the directory says there is a 256 KiB file" is not the same
//! as "there is a 256 KiB file". [`Volume::files`] therefore validates the
//! whole cluster chain against the recorded size and skips anything that does
//! not add up, and the firmware re-runs it after the volume goes quiet rather
//! than trying to follow the write stream. See [`Volume::files`].
//!
//! Long filenames are not optional either: Linux writes a VFAT long-name entry
//! even for a plain `blink.py`, because the short name it stores alongside is
//! uppercased to `BLINK.PY`. Reading only the 8.3 name would show the user a
//! name they did not choose.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod format;
pub mod read;

pub use format::{VOLUME_BYTES, format};
pub use read::{Error, FileInfo, Volume};

/// The badge's volume geometry. Only [`format`] uses these; the reader takes
/// whatever the BPB says, so a volume the host has reformatted still works.
///
/// FAT12 is not a choice so much as an arithmetic consequence. The type is
/// selected by cluster count -- under 4085 clusters is FAT12 by definition --
/// and 4085 clusters of 512 bytes needs a volume over 2 MiB. At 512 KiB there
/// is no other legal option, whatever string the boot sector claims.
pub mod geom {
    pub const BYTES_PER_SECTOR: usize = 512;
    pub const SECTORS_PER_CLUSTER: usize = 1;
    pub const RESERVED_SECTORS: usize = 1;
    pub const NUM_FATS: usize = 2;
    /// 3 sectors = 1536 bytes = 1024 FAT12 entries, comfortably over the
    /// `cluster_count + 2` the volume needs.
    pub const FAT_SECTORS: usize = 3;
    /// 64 entries x 32 bytes = 2048 bytes = 4 sectors. Each file costs one
    /// entry for its 8.3 name plus one per 13 characters of long name, so this
    /// holds roughly 20 scripts with comfortable names.
    pub const ROOT_ENTRIES: usize = 64;
    pub const TOTAL_SECTORS: usize = 1024;

    pub const ROOT_SECTORS: usize = ROOT_ENTRIES * 32 / BYTES_PER_SECTOR;
    pub const FAT_START: usize = RESERVED_SECTORS;
    pub const ROOT_START: usize = FAT_START + NUM_FATS * FAT_SECTORS;
    pub const DATA_START: usize = ROOT_START + ROOT_SECTORS;
    pub const CLUSTER_COUNT: usize = (TOTAL_SECTORS - DATA_START) / SECTORS_PER_CLUSTER;
}

// Directory entry attribute bits.
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
/// The four low bits together mark a long-filename fragment. It is deliberately
/// a combination no real file can have, so old DOS tools skip them.
pub const ATTR_LFN: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

/// The checksum a long-name entry carries, computed over the 11-byte short name
/// of the entry it belongs to. A mismatch means the long name is stale -- some
/// tool rewrote the short entry without updating the long ones -- and the whole
/// long name must be discarded.
pub fn sfn_checksum(sfn: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in sfn.iter() {
        sum = (if sum & 1 != 0 { 0x80u8 } else { 0u8 }).wrapping_add(sum >> 1).wrapping_add(c);
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    // Asserting on constants is the point: these are compile-time geometry
    // facts, and the test exists to fail the moment someone edits `geom` into a
    // volume that is no longer legally FAT12.
    #[test]
    #[allow(clippy::assertions_on_constants, reason = "checking the constants *is* the test")]
    fn geometry_lands_in_the_fat12_band() {
        // Under 4085 clusters is what makes this FAT12 rather than a FAT16
        // volume that only claims to be one.
        assert!(geom::CLUSTER_COUNT < 4085, "{} clusters", geom::CLUSTER_COUNT);
        // The FAT has to hold an entry per cluster plus the two reserved ones.
        let entries = geom::FAT_SECTORS * geom::BYTES_PER_SECTOR * 8 / 12;
        assert!(
            entries >= geom::CLUSTER_COUNT + 2,
            "FAT too small: {} < {}",
            entries,
            geom::CLUSTER_COUNT + 2
        );
    }

    #[test]
    fn checksum_matches_values_observed_from_a_real_host() {
        // Both captured from directory entries a Linux host wrote.
        assert_eq!(sfn_checksum(b"BLINK   PY "), 0x27);
        assert_eq!(sfn_checksum(b"MYCOOL~1PY "), 0x16);
    }
}
