//! Small helpers that keep the rest of the firmware allocation-free.

use core::fmt::Write;

/// A fixed-capacity sink for `write!`, so screens can format numbers into a
/// `&str` without touching the heap.
///
/// A fragment that would not fit is dropped whole rather than truncated, which
/// is what keeps [`FmtBuf::as_str`] from ever seeing a split UTF-8 sequence.
pub struct FmtBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FmtBuf<N> {
    pub const fn new() -> Self { Self { buf: [0u8; N], len: 0 } }

    pub fn as_str(&self) -> &str { core::str::from_utf8(&self.buf[..self.len]).unwrap_or("") }

    pub fn clear(&mut self) { self.len = 0; }

    /// Format `args` into a cleared buffer and hand back the result.
    pub fn format(&mut self, args: core::fmt::Arguments<'_>) -> &str {
        self.clear();
        let _ = self.write_fmt(args);
        self.as_str()
    }
}

impl<const N: usize> Write for FmtBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.len + s.len() > N {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

/// xorshift32. Not cryptographic, and deliberately not seeded from the TRNG --
/// this firmware never powers that block up. It only has to make the animations
/// look organic.
pub struct Rng(u32);

impl Rng {
    pub const fn new(seed: u32) -> Self { Rng(if seed == 0 { 0x1379_2a5b } else { seed }) }

    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform enough over `0..n`. Panics if `n == 0`.
    pub fn below(&mut self, n: u32) -> u32 { self.next() % n }
}

/// Stateless integer hash, used to pick a glyph for a cell from its coordinates
/// so the character stays put between frames instead of flickering every redraw.
pub fn hash3(a: u32, b: u32, c: u32) -> u32 {
    let mut x = a.wrapping_mul(0x9e37_79b9) ^ b.wrapping_mul(0x85eb_ca6b) ^ c.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2545_f491);
    x ^= x >> 13;
    x
}
