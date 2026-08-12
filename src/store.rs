//! Keeping the script volume across power cycles, in on-chip ReRAM.
//!
//! # Why ReRAM and not the SPI flash
//!
//! The badge has both: 4 MiB of on-chip ReRAM and an 8 MiB off-chip SPI NOR
//! part. ReRAM wins here on three counts. It is bit-alterable, so there is no
//! erase-before-write cycle to coordinate; it is memory-mapped, so verifying
//! what is already stored is a `memcmp` rather than a DMA read into a buffer we
//! would have to find room for; and it needs no peripheral bring-up at all,
//! where the SPI part needs pins, a UDMA channel, a QPI mode negotiation and
//! two pages of IFRAM.
//!
//! # Where, and why that is safe
//!
//! `0x6020_0000..0x6028_1000`. The ReRAM write path refuses anything below
//! `BOOT1_START` or at or above `RRAM_STORAGE_LEN`, and this window sits well
//! inside those bounds -- but it is checked again here, because the HAL's limit
//! still permits writing over boot1 itself, and a bug in an offset calculation
//! should not be able to reach it.
//!
//! The region is far above this firmware's own image (which ends by
//! `0x6016_0000` at the linker's limit) and far below the one-way counter page
//! at `0x603D_A000`, where an ordinary store of zero *is* a counter increment.
//! It is not covered by the image signature -- boot0 and boot1 hash only the
//! signed length recorded in the header -- so writing here cannot invalidate
//! the firmware, and nothing on the boot path erases it.
//!
//! On a stock badge this address holds part of the Xous kernel. That is not a
//! conflict: a baremetal image replaces the loader, and Xous is not running.
//! Re-flashing the stock firmware overwrites this region and takes the scripts
//! with it, which is the correct outcome.

use bao1x_hal::rram::Reram;

use crate::usb::msc;

/// Start of the persistent area, as an offset from the base of ReRAM.
const STORE_OFF: usize = 0x0020_0000;
/// One page of header, then the volume image.
const HEADER_LEN: usize = 4096;
const VOLUME_OFF: usize = STORE_OFF + HEADER_LEN;
const STORE_LEN: usize = HEADER_LEN + msc::DISK_BYTES;

/// Hard bound on every write this module makes. The HAL's own check is wider
/// than this region, so it is not enough on its own.
const STORE_END: usize = STORE_OFF + STORE_LEN;

/// Bumped when the on-ReRAM layout changes, so an old store is ignored rather
/// than misread.
const MAGIC: [u8; 8] = *b"BADGYVOL";

/// Granularity of the write-back scan. ReRAM programs in 32-byte units, so this
/// is about limiting the number of `write_slice` calls, not about erase blocks.
const CHUNK: usize = 4096;

/// A read-only view of the stored bytes. ReRAM is memory-mapped, so this is
/// just a pointer.
///
/// Only safe to hold while no [`Reram`] exists: `Reram::new()` builds its own
/// `&'static mut [u32]` over the whole array, and two live references to the
/// same bytes -- one of them mutable -- is exactly the aliasing the compiler is
/// entitled to assume never happens. Code that runs alongside a `Reram` uses
/// [`stored_matches`] instead.
fn stored() -> &'static [u8] {
    // safety: the range is inside the ReRAM array, and no `Reram` is alive at
    // any call site of this function.
    unsafe { core::slice::from_raw_parts((utralib::HW_RERAM_MEM + STORE_OFF) as *const u8, STORE_LEN) }
}

/// Compare `data` against what is stored at `offset` bytes into the store,
/// without materializing a reference that would alias `Reram`'s.
fn stored_matches(offset: usize, data: &[u8]) -> bool {
    let base = (utralib::HW_RERAM_MEM + STORE_OFF + offset) as *const u8;
    // safety: raw reads inside the store window, one byte at a time, so no
    // reference to the region is ever created.
    data.iter().enumerate().all(|(i, b)| unsafe { base.add(i).read_volatile() } == *b)
}

/// Header layout: magic, then the volume length, then its checksum.
fn parse_header(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 16 || h[..8] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
    let crc = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
    Some((len, crc))
}

/// Is there a volume in ReRAM that matches its own checksum?
pub fn has_saved_volume() -> bool {
    let s = stored();
    let Some((len, crc)) = parse_header(&s[..16]) else {
        return false;
    };
    len as usize == msc::DISK_BYTES && crc32(&s[HEADER_LEN..HEADER_LEN + len as usize]) == crc
}

/// Copy the saved volume into the RAM disk. Returns false if there was nothing
/// valid to restore, in which case the caller should format a fresh one.
pub fn load() -> bool {
    if !has_saved_volume() {
        return false;
    }
    let s = stored();
    msc::disk().copy_from_slice(&s[HEADER_LEN..HEADER_LEN + msc::DISK_BYTES]);
    true
}

/// Write the RAM disk back, skipping anything that already matches.
///
/// Returns the number of bytes actually programmed, which is zero when nothing
/// changed. The compare-first pass is what keeps a `save()` after an idle
/// mount from rewriting half a megabyte -- a host that only touched a directory
/// sector costs one 4 KiB program.
pub fn save() -> usize {
    let disk = msc::disk();
    let mut rram = Reram::new();
    let mut written = 0usize;

    for (i, chunk) in disk.chunks(CHUNK).enumerate() {
        let off = VOLUME_OFF + i * CHUNK;
        debug_assert!(off + chunk.len() <= STORE_END);
        if off + chunk.len() > STORE_END {
            break;
        }
        if stored_matches(HEADER_LEN + i * CHUNK, chunk) {
            continue;
        }
        if write_bounded(&mut rram, off, chunk).is_err() {
            crate::println!("store: write failed at offset {:#x}", off);
            return written;
        }
        written += chunk.len();
    }

    // The header goes last and carries the checksum, so a save interrupted by
    // a power loss leaves the old header pointing at a body that no longer
    // matches it -- which fails verification and falls back to a fresh format,
    // rather than restoring a half-written volume.
    let mut header = [0u8; 16];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&(msc::DISK_BYTES as u32).to_le_bytes());
    header[12..16].copy_from_slice(&crc32(disk).to_le_bytes());
    if write_bounded(&mut rram, STORE_OFF, &header).is_err() {
        crate::println!("store: header write failed");
    }
    written
}

/// Erase the store, so the next boot formats a fresh volume.
pub fn clear() {
    let mut rram = Reram::new();
    let zero = [0u8; 32];
    let _ = write_bounded(&mut rram, STORE_OFF, &zero);
}

/// Every write goes through here. The HAL will happily write to boot1's own
/// partition -- its bound is `BOOT1_START..RRAM_STORAGE_LEN` -- so this is the
/// check that actually confines us.
fn write_bounded(rram: &mut Reram, offset: usize, data: &[u8]) -> Result<(), ()> {
    if offset < STORE_OFF || offset.saturating_add(data.len()) > STORE_END {
        crate::println!("store: refusing out-of-range write at {:#x}+{}", offset, data.len());
        return Err(());
    }
    rram.write_slice(offset, data).map(|_| ()).map_err(|_| ())
}

/// CRC-32 (the usual reflected polynomial), computed without a lookup table.
///
/// A 1 KiB table would be faster but would land in `.data` or `.rodata` for no
/// good reason: this runs over 512 KiB perhaps once a minute, and the bitwise
/// form costs a few milliseconds.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_reference_check_value() {
        // The standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
