//! Parsing a FAT volume out of a flat byte slice.

use alloc::string::String;
use alloc::vec::Vec;

use crate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No `0x55AA` at offset 510, or the BPB is self-contradictory.
    NotFat,
    /// A geometry this reader deliberately does not handle -- a sector size
    /// other than 512, or a cluster count in the FAT32 band.
    Unsupported,
    /// The volume slice is shorter than the boot sector says it should be.
    Truncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatKind {
    Fat12,
    Fat16,
}

#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub num_fats: u32,
    pub fat_sectors: u32,
    pub root_entries: u32,
    pub total_sectors: u32,
    /// First sector of FAT #1.
    pub fat_start: u32,
    /// First sector of the root directory region.
    pub root_start: u32,
    /// First sector of the cluster area, i.e. where cluster 2 begins.
    pub data_start: u32,
    /// Number of *data* clusters. Valid cluster numbers are `2..cluster_count + 2`.
    pub cluster_count: u32,
    pub kind: FatKind,
}

impl Geometry {
    pub fn bytes_per_cluster(&self) -> u32 { self.bytes_per_sector * self.sectors_per_cluster }

    /// Byte offset of a cluster's data.
    fn cluster_offset(&self, cluster: u32) -> u64 {
        (self.data_start as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64)
            * self.bytes_per_sector as u64
    }
}

/// A file the badge is prepared to show. Names are already decoded from the
/// long-name entries where present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub first_cluster: u32,
    pub size: u32,
}

impl FileInfo {
    /// Case-insensitive extension test, for picking `.py` out of whatever else
    /// the host decided to leave lying around.
    pub fn has_extension(&self, ext: &str) -> bool {
        let Some(dot) = self.name.rfind('.') else {
            return false;
        };
        self.name[dot + 1..].eq_ignore_ascii_case(ext)
    }
}

pub struct Volume<'a> {
    data: &'a [u8],
    pub geom: Geometry,
}

/// Prints the layout, not the half-megabyte of bytes behind it.
impl core::fmt::Debug for Volume<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Volume({:?}, {} sectors, {} clusters of {} B)",
            self.geom.kind,
            self.geom.total_sectors,
            self.geom.cluster_count,
            self.geom.bytes_per_cluster()
        )
    }
}

/// Cap on how many entries a directory walk will look at, and how many links a
/// cluster chain may have, independent of what the volume claims. A corrupt or
/// hostile FAT can describe a cycle; these bound the work regardless.
const MAX_DIR_ENTRIES: u32 = 4096;

/// Longest long-name this reader will assemble. 20 LFN entries x 13 characters
/// is the format's own maximum; anything claiming more is corrupt.
const MAX_LFN_CHARS: usize = 260;

impl<'a> Volume<'a> {
    /// Read the boot sector and derive the layout. Rejects anything it cannot
    /// safely index rather than trusting the numbers.
    pub fn open(data: &'a [u8]) -> Result<Volume<'a>, Error> {
        if data.len() < 512 {
            return Err(Error::Truncated);
        }
        if data[510] != 0x55 || data[511] != 0xAA {
            return Err(Error::NotFat);
        }

        let rd16 = |off: usize| u16::from_le_bytes([data[off], data[off + 1]]) as u32;
        let rd32 = |off: usize| u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);

        let bytes_per_sector = rd16(0x0B);
        let sectors_per_cluster = data[0x0D] as u32;
        let reserved_sectors = rd16(0x0E);
        let num_fats = data[0x10] as u32;
        let root_entries = rd16(0x11);
        let total_16 = rd16(0x13);
        let fat_sectors = rd16(0x16);
        let total_32 = rd32(0x20);
        let total_sectors = if total_16 != 0 { total_16 } else { total_32 };

        // The block layer underneath is 512-byte SCSI sectors, so a volume with
        // any other sector size could not have got here intact.
        if bytes_per_sector != 512 {
            return Err(Error::Unsupported);
        }
        if !sectors_per_cluster.is_power_of_two() || sectors_per_cluster > 64 {
            return Err(Error::NotFat);
        }
        if reserved_sectors == 0 || !(1..=2).contains(&num_fats) || fat_sectors == 0 {
            return Err(Error::NotFat);
        }
        // FAT32 puts 0 here and keeps its root directory in a cluster chain;
        // that is a different layout and this reader does not implement it.
        if root_entries == 0 {
            return Err(Error::Unsupported);
        }
        if (root_entries * 32) % bytes_per_sector != 0 {
            return Err(Error::NotFat);
        }
        if total_sectors == 0 {
            return Err(Error::NotFat);
        }

        let root_sectors = root_entries * 32 / bytes_per_sector;
        let fat_start = reserved_sectors;
        let root_start = fat_start + num_fats * fat_sectors;
        let data_start = root_start + root_sectors;
        if data_start >= total_sectors {
            return Err(Error::NotFat);
        }
        let cluster_count = (total_sectors - data_start) / sectors_per_cluster;

        // The type is decided by the cluster count, never by the "FAT12   "
        // string in the boot sector -- that field is advisory and is wrong on
        // plenty of real volumes.
        let kind = if cluster_count < 4085 {
            FatKind::Fat12
        } else if cluster_count < 65525 {
            FatKind::Fat16
        } else {
            return Err(Error::Unsupported);
        };

        // Everything the layout describes must be inside the slice we were
        // given, so every later index can be a plain slice op.
        let need = total_sectors as u64 * bytes_per_sector as u64;
        if (data.len() as u64) < need {
            return Err(Error::Truncated);
        }
        // ...and the FAT must be big enough for the clusters it indexes.
        let fat_bits = fat_sectors as u64 * bytes_per_sector as u64 * 8;
        let need_bits = (cluster_count as u64 + 2) * if kind == FatKind::Fat12 { 12 } else { 16 };
        if fat_bits < need_bits {
            return Err(Error::NotFat);
        }

        Ok(Volume {
            data,
            geom: Geometry {
                bytes_per_sector,
                sectors_per_cluster,
                num_fats,
                fat_sectors,
                root_entries,
                total_sectors,
                fat_start,
                root_start,
                data_start,
                cluster_count,
                kind,
            },
        })
    }

    /// The 11-byte volume label from the root directory, if one is there.
    /// Falls back to the boot sector's copy, which is what tools write but not
    /// what hosts display.
    pub fn label(&self) -> String {
        for i in 0..self.geom.root_entries {
            let e = self.dir_entry(i);
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xE5 {
                continue;
            }
            if e[11] == ATTR_LFN {
                continue;
            }
            if e[11] & ATTR_VOLUME_ID != 0 {
                return trim_label(&e[..11]);
            }
        }
        trim_label(&self.data[0x2B..0x36])
    }

    /// Every complete, plausible regular file in the root directory.
    ///
    /// "Complete" is the load-bearing word. A directory entry appears before
    /// its data does, and with intermediate sizes along the way, so an entry is
    /// only reported once its cluster chain has exactly as many links as its
    /// recorded size needs and terminates properly. A file still being written
    /// simply does not appear yet, and shows up on the next scan.
    ///
    /// Directories, the volume label and hidden/system entries are skipped --
    /// which is also how `System Volume Information`, `.Spotlight-V100` and the
    /// rest of the junk hosts leave behind stay out of the badge's menu.
    pub fn files(&self) -> Vec<FileInfo> {
        let mut out = Vec::new();
        // Long-name fragments arrive *before* the short entry they describe,
        // in reverse order, so they accumulate here until the short entry
        // arrives to claim (and validate) them.
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_checksum: Option<u8> = None;
        let mut expect_seq: u8 = 0;

        let limit = self.geom.root_entries.min(MAX_DIR_ENTRIES);
        for i in 0..limit {
            let e = self.dir_entry(i);

            // 0x00 means this entry and every one after it is free.
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xE5 {
                lfn.clear();
                lfn_checksum = None;
                continue;
            }

            if e[11] == ATTR_LFN {
                let order = e[0];
                let last = order & 0x40 != 0;
                let seq = order & 0x1F;
                // A well-formed set counts down to 1. Anything else means we
                // joined mid-set or the directory is damaged; drop it all.
                if last {
                    lfn.clear();
                    lfn_checksum = Some(e[13]);
                    expect_seq = seq;
                } else if lfn_checksum != Some(e[13]) || seq != expect_seq {
                    lfn.clear();
                    lfn_checksum = None;
                    continue;
                }
                if seq == 0 || seq > 20 {
                    lfn.clear();
                    lfn_checksum = None;
                    continue;
                }
                expect_seq = seq.saturating_sub(1);

                // 13 UTF-16 code units, split across three disjoint runs
                // because the layout has to dodge the fields that mean
                // something else in a short entry.
                let mut chunk = [0u16; 13];
                for (k, dst) in chunk.iter_mut().enumerate() {
                    let off = match k {
                        0..=4 => 1 + k * 2,
                        5..=10 => 14 + (k - 5) * 2,
                        _ => 28 + (k - 11) * 2,
                    };
                    *dst = u16::from_le_bytes([e[off], e[off + 1]]);
                }
                // Fragments are stored highest-sequence-first, so prepend.
                let mut merged = Vec::with_capacity(lfn.len() + 13);
                merged.extend_from_slice(&chunk);
                merged.extend_from_slice(&lfn);
                lfn = merged;
                if lfn.len() > MAX_LFN_CHARS {
                    lfn.clear();
                    lfn_checksum = None;
                }
                continue;
            }

            let attr = e[11];
            let skip = attr & (ATTR_VOLUME_ID | ATTR_DIRECTORY | ATTR_HIDDEN | ATTR_SYSTEM) != 0;

            let mut sfn = [0u8; 11];
            sfn.copy_from_slice(&e[..11]);
            // 0x05 stands in for a leading 0xE5 in a real name, since 0xE5 in
            // byte 0 means "deleted".
            if sfn[0] == 0x05 {
                sfn[0] = 0xE5;
            }

            let name = match (lfn_checksum, expect_seq) {
                // A complete set, whose checksum agrees with this short name.
                (Some(sum), 0) if sum == sfn_checksum(&sfn) && !lfn.is_empty() => {
                    decode_lfn(&lfn).unwrap_or_else(|| short_name(&sfn, e[12]))
                }
                _ => short_name(&sfn, e[12]),
            };
            lfn.clear();
            lfn_checksum = None;

            if skip || name.is_empty() {
                continue;
            }

            let first_cluster = u16::from_le_bytes([e[26], e[27]]) as u32;
            let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);

            if self.chain_is_complete(first_cluster, size) {
                out.push(FileInfo { name, first_cluster, size });
            }
        }
        out
    }

    /// Copy a file's bytes out. Returns `None` if the chain does not check out,
    /// which can happen if the volume changed between [`Volume::files`] and here.
    pub fn read_file(&self, f: &FileInfo) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(f.size as usize);
        let bpc = self.geom.bytes_per_cluster() as usize;
        let mut cluster = f.first_cluster;
        let mut left = f.size as usize;
        let mut guard = self.geom.cluster_count + 2;

        while left > 0 {
            if !self.cluster_is_valid(cluster) || guard == 0 {
                return None;
            }
            guard -= 1;
            let off = self.geom.cluster_offset(cluster) as usize;
            let take = left.min(bpc);
            out.extend_from_slice(self.data.get(off..off + take)?);
            left -= take;
            if left == 0 {
                break;
            }
            cluster = self.fat_entry(cluster)?;
        }
        Some(out)
    }

    /// Bytes currently used by files, and bytes free, for the info screen.
    pub fn usage(&self) -> (u32, u32) {
        let mut free = 0u32;
        for c in 2..self.geom.cluster_count + 2 {
            if self.fat_entry(c) == Some(0) {
                free += 1;
            }
        }
        let bpc = self.geom.bytes_per_cluster();
        (self.geom.cluster_count.saturating_sub(free) * bpc, free * bpc)
    }

    // ------------------------------------------------------------------ internals

    fn dir_entry(&self, index: u32) -> &[u8] {
        let off = self.geom.root_start as usize * self.geom.bytes_per_sector as usize + index as usize * 32;
        &self.data[off..off + 32]
    }

    fn cluster_is_valid(&self, cluster: u32) -> bool { cluster >= 2 && cluster < self.geom.cluster_count + 2 }

    /// The FAT entry for `cluster`, or `None` if the read would be out of range.
    fn fat_entry(&self, cluster: u32) -> Option<u32> {
        let fat_base = self.geom.fat_start as usize * self.geom.bytes_per_sector as usize;
        match self.geom.kind {
            FatKind::Fat12 => {
                // 12-bit entries: entry n starts at byte n + n/2, and whether
                // it is the low or high nibble of the shared byte depends on
                // the parity of n.
                let k = fat_base + cluster as usize + (cluster as usize / 2);
                let lo = *self.data.get(k)? as u32;
                let hi = *self.data.get(k + 1)? as u32;
                Some(if cluster & 1 == 0 { ((hi & 0x0F) << 8) | lo } else { (hi << 4) | (lo >> 4) })
            }
            FatKind::Fat16 => {
                let k = fat_base + cluster as usize * 2;
                let lo = *self.data.get(k)? as u32;
                let hi = *self.data.get(k + 1)? as u32;
                Some((hi << 8) | lo)
            }
        }
    }

    fn is_eoc(&self, entry: u32) -> bool {
        match self.geom.kind {
            FatKind::Fat12 => entry >= 0xFF8,
            FatKind::Fat16 => entry >= 0xFFF8,
        }
    }

    /// Does the chain starting at `first` hold exactly `size` bytes and stop?
    ///
    /// This is the check that keeps a half-copied file out of the menu. A host
    /// writes the directory entry before the data, so an entry can advertise a
    /// size whose clusters have not been allocated yet -- the chain then ends
    /// early, or runs on past where it should, and either way fails here.
    fn chain_is_complete(&self, first: u32, size: u32) -> bool {
        let bpc = self.geom.bytes_per_cluster();
        if size == 0 {
            // An empty file is allowed to have no chain at all. Some hosts
            // leave a stale cluster number behind; either is fine.
            return true;
        }
        let want = size.div_ceil(bpc);
        // A file cannot occupy more clusters than the volume has. Checking this
        // up front is not just tidiness: without it, a directory entry claiming
        // a 4 GB size sends the walk below round a cyclic chain eight million
        // times before it gives up, and there are 64 directory entries.
        if want > self.geom.cluster_count {
            return false;
        }
        let mut cluster = first;
        for step in 0..want {
            if !self.cluster_is_valid(cluster) {
                return false;
            }
            let Some(next) = self.fat_entry(cluster) else {
                return false;
            };
            let last = step + 1 == want;
            if last {
                // The final cluster must terminate the chain. If it points at
                // another cluster the file is longer than it claims, which
                // means we caught it mid-write.
                return self.is_eoc(next);
            }
            if self.is_eoc(next) || next == 0 {
                return false;
            }
            cluster = next;
        }
        false
    }
}

/// Render an 8.3 name, honouring the two flag bits Windows added for case.
///
/// Without them a host that stored `blink.py` as `BLINK   PY ` would be shown
/// back to the user as `BLINK.PY`. Linux does not set the flags, so a file with
/// no long-name entry really can only be displayed in upper case -- but in
/// practice Linux always writes the long-name entry too.
fn short_name(sfn: &[u8; 11], nt_flags: u8) -> String {
    let base_lower = nt_flags & 0x08 != 0;
    let ext_lower = nt_flags & 0x10 != 0;
    let mut out = String::new();
    for &c in &sfn[..8] {
        if c == b' ' {
            break;
        }
        out.push(map_case(c, base_lower));
    }
    let ext_start = out.len();
    let _ = ext_start;
    let mut ext = String::new();
    for &c in &sfn[8..] {
        if c == b' ' {
            break;
        }
        ext.push(map_case(c, ext_lower));
    }
    if !ext.is_empty() {
        out.push('.');
        out.push_str(&ext);
    }
    out
}

fn map_case(c: u8, lower: bool) -> char {
    let c = if lower { c.to_ascii_lowercase() } else { c };
    // Anything outside printable ASCII would be an OEM codepage character we
    // have no font for; show it as '?' rather than mangling the UTF-8.
    if (0x20..0x7F).contains(&c) { c as char } else { '?' }
}

/// UTF-16LE fragments -> a `String`, stopping at the end of the name.
///
/// Two things end it. A `0x0000` terminator is written whenever the name does
/// not exactly fill its last entry; when it *does* fill it there is no
/// terminator at all and the name simply stops at the entry boundary, with the
/// remaining slots left as `0xFFFF` padding. Treating both as the end is what
/// keeps a 13-, 26- or 39-character name from picking up a tail of U+FFFF.
///
/// Returns `None` if the units do not decode, which sends the caller back to
/// the short name rather than showing mojibake.
fn decode_lfn(units: &[u16]) -> Option<String> {
    let end = units.iter().position(|&u| u == 0 || u == 0xFFFF).unwrap_or(units.len());
    let units = &units[..end];
    if units.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        // A lone surrogate means the name is not valid UTF-16 at all.
        out.push(r.ok()?);
    }
    Some(out)
}

fn trim_label(raw: &[u8]) -> String {
    let mut out = String::new();
    for &c in raw {
        out.push(map_case(c, false));
    }
    String::from(out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_long_name() {
        let units: Vec<u16> = "hello.py".encode_utf16().chain(core::iter::once(0)).collect();
        assert_eq!(decode_lfn(&units).unwrap(), "hello.py");
    }

    #[test]
    fn short_name_case_flags() {
        assert_eq!(short_name(b"BLINK   PY ", 0x00), "BLINK.PY");
        assert_eq!(short_name(b"BLINK   PY ", 0x18), "blink.py");
        assert_eq!(short_name(b"NOEXT      ", 0x00), "NOEXT");
    }

    #[test]
    fn extension_test_is_case_insensitive() {
        let f = |n: &str| FileInfo { name: n.into(), first_cluster: 2, size: 1 };
        assert!(f("a.py").has_extension("py"));
        assert!(f("a.PY").has_extension("py"));
        assert!(!f("apy").has_extension("py"));
        assert!(!f("a.pyc").has_extension("py"));
    }

    #[test]
    fn rejects_a_slice_that_is_not_a_filesystem() {
        assert_eq!(Volume::open(&[0u8; 512]).unwrap_err(), Error::NotFat);
        assert_eq!(Volume::open(&[0u8; 16]).unwrap_err(), Error::Truncated);
    }
}
