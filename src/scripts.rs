//! The bridge between the USB drive and the menu: what `.py` files are on the
//! volume right now, and when to go and look.
//!
//! # When to look
//!
//! A host does not tell us it has finished copying a file. It writes the
//! directory entry first, then the data, out of order, and revises the
//! directory entry as it goes -- so at almost any instant during a copy the
//! volume describes a file that does not fully exist yet. Two things guard
//! against importing one of those:
//!
//! * the scan only runs once the write traffic has stopped for [`IDLE_POLLS`] passes of the main loop, or
//!   immediately when the host sends an explicit flush (`SYNCHRONIZE CACHE`, or the eject half of `PREVENT
//!   ALLOW MEDIUM REMOVAL`);
//! * [`badgy_fat::Volume::files`] independently refuses any file whose cluster chain does not match its
//!   recorded size, so a copy caught mid-flight is skipped and picked up on the next scan.
//!
//! Idle is counted in loop passes rather than milliseconds on purpose. There is
//! no free-running clock in this firmware: `timer0` is an auto-reload with a
//! sticky one-bit event flag, and a single panel refresh blocks past a dozen of
//! them, so a millisecond count taken from a render loop reads low and does so
//! unpredictably. Loop passes are what the rest of the firmware already times
//! in.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use badgy_fat::{FileInfo, Volume};

use crate::store;
use crate::usb::msc;

/// Quiet passes of the main loop before a rescan. At roughly 4 ms a pass this
/// is about half a second -- long enough that a `cp` is not interrupted
/// mid-flight, short enough that the menu updates while the user is still
/// looking at the badge.
const IDLE_POLLS: u32 = 128;

/// Longest a rescan will be deferred by continuing write traffic.
///
/// The idle wait exists so a scan does not land mid-copy, but a host that keeps
/// writing -- a very large file, or one that simply never stops -- would defer
/// it forever and the menu would never update. Forcing a scan is safe because
/// `Volume::files` refuses any file whose cluster chain does not match its
/// recorded size, so a copy still in flight is skipped rather than imported
/// half-finished. At about 4 ms a pass this is roughly twenty seconds.
const MAX_DEFER_POLLS: u32 = 5000;

/// Most scripts the menu will show. The root directory holds 64 entries and a
/// long filename eats several, so this is not a limit anyone reaches -- it is
/// here because the menu action carries the index in a `u8`.
const MAX_SCRIPTS: usize = 200;

/// Longest script the badge will load.
///
/// This is a memory limit, measured rather than guessed. Lexing and parsing
/// hold the token vector and the AST arena at the same time, which costs about
/// 25 KB per KiB of source on rv32 -- so 16 KiB of Python peaks near 250 KB of
/// a 768 KiB heap, leaving room for what the script itself allocates. It is
/// also around 500 lines, which is a great deal of Python for a 128x128 screen.
///
/// A larger file is skipped with a note on the console rather than loaded and
/// failed halfway: an allocation failure would panic, and a panic here spins
/// forever.
pub const MAX_SCRIPT_BYTES: usize = 16 * 1024;

/// What the drive looked like at the last scan.
pub struct Scripts {
    files: Vec<FileInfo>,
    /// Value of [`msc::WRITE_COUNT`] at the last check, to spot new writes.
    seen_writes: u32,
    /// Loop passes since the last write.
    quiet: u32,
    /// Loop passes since a rescan was first owed, however busy the drive is.
    deferred: u32,
    /// Set while a rescan is owed.
    pending: bool,
    /// Bumped on every rescan that changed the list, so the UI can repaint.
    pub generation: u32,
    /// Bytes used and free, refreshed with the list.
    pub used: u32,
    pub free: u32,
}

impl Scripts {
    pub const fn new() -> Self {
        Scripts {
            files: Vec::new(),
            seen_writes: 0,
            quiet: 0,
            deferred: 0,
            pending: false,
            generation: 0,
            used: 0,
            free: 0,
        }
    }

    /// Restore the volume from ReRAM, or lay down a fresh one, then take an
    /// initial inventory.
    pub fn init(&mut self) {
        if store::load() {
            crate::println!("scripts: restored the volume from ReRAM");
        } else {
            crate::println!("scripts: no saved volume, formatting");
            self.format_fresh();
            store::save();
        }
        // A volume that will not parse is worse than no volume: the host would
        // mount it, fail, and offer to reformat. Start over instead.
        if Volume::open(msc::disk()).is_err() {
            crate::println!("scripts: stored volume did not parse, reformatting");
            self.format_fresh();
            store::save();
        }
        self.rescan();
    }

    /// Called once per pass of the main loop.
    ///
    /// Returns true if the script list changed, which the caller uses to
    /// repaint a menu that might be showing it.
    pub fn poll(&mut self) -> bool {
        let writes = msc::WRITE_COUNT.load(Ordering::SeqCst);
        if writes != self.seen_writes {
            self.seen_writes = writes;
            self.quiet = 0;
            if !self.pending {
                self.deferred = 0;
            }
            self.pending = true;
            self.deferred += 1;
            // Do not let a busy drive postpone the scan indefinitely.
            if self.deferred >= MAX_DEFER_POLLS {
                return self.commit();
            }
            return false;
        }

        // An explicit flush skips the wait: the host has told us it is done.
        if msc::FLUSH_REQUESTED.swap(false, Ordering::SeqCst) && self.pending {
            return self.commit();
        }

        if self.pending {
            self.quiet += 1;
            if self.quiet >= IDLE_POLLS {
                return self.commit();
            }
        }
        false
    }

    /// Re-read the volume and write it back to ReRAM.
    fn commit(&mut self) -> bool {
        self.pending = false;
        self.quiet = 0;
        self.deferred = 0;
        let changed = self.rescan();
        let written = store::save();
        if written > 0 {
            crate::println!("scripts: saved {} bytes to ReRAM", written);
        }
        changed
    }

    /// Take an inventory of the volume. Returns true if the list differs from
    /// what we had.
    pub fn rescan(&mut self) -> bool {
        let before: Vec<String> = self.files.iter().map(|f| f.name.clone()).collect();

        self.files.clear();
        self.used = 0;
        self.free = 0;
        if let Ok(vol) = Volume::open(msc::disk()) {
            let (used, free) = vol.usage();
            self.used = used;
            self.free = free;
            for f in vol.files() {
                if self.files.len() >= MAX_SCRIPTS {
                    crate::println!("scripts: more than {} scripts, ignoring the rest", MAX_SCRIPTS);
                    break;
                }
                if !f.has_extension("py") {
                    continue;
                }
                if f.size as usize > MAX_SCRIPT_BYTES {
                    crate::println!("scripts: {} is {} bytes, too big to load", f.name, f.size);
                    continue;
                }
                self.files.push(f);
            }
            // Alphabetical, so the menu does not reshuffle itself every time
            // the host rewrites the directory.
            self.files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }

        let after: Vec<String> = self.files.iter().map(|f| f.name.clone()).collect();
        let changed = before != after;
        if changed {
            self.generation = self.generation.wrapping_add(1);
            crate::println!("scripts: {} script(s) on the drive", self.files.len());
            for f in &self.files {
                crate::println!("  {} ({} bytes)", f.name, f.size);
            }
        }
        changed
    }

    /// True while a host is mid-copy: writes have arrived and the rescan is
    /// waiting for the drive to go quiet. What Badgy shows as digging.
    pub fn busy(&self) -> bool { self.pending }

    pub fn len(&self) -> usize { self.files.len() }

    pub fn is_empty(&self) -> bool { self.files.is_empty() }

    pub fn name(&self, i: usize) -> &str { self.files.get(i).map(|f| f.name.as_str()).unwrap_or("") }

    /// Read a script's source.
    ///
    /// Re-opens the volume rather than caching bytes: the drive may have been
    /// rewritten since the scan, and stale source would run something the user
    /// did not ask for.
    pub fn source(&self, i: usize) -> Option<String> {
        let f = self.files.get(i)?;
        let vol = Volume::open(msc::disk()).ok()?;
        // Match by name: cluster numbers can move if the host rewrote the file.
        let live = vol.files().into_iter().find(|c| c.name == f.name)?;
        // Re-check the size: the drive may have been rewritten since the scan,
        // and the limit exists to keep the parse inside the heap.
        if live.size as usize > MAX_SCRIPT_BYTES {
            return None;
        }
        let bytes = vol.read_file(&live)?;
        // A script is text. Anything that is not valid UTF-8 becomes a
        // replacement character, which the tokenizer then rejects with a
        // sensible message instead of a panic.
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Lay down a blank volume with a couple of examples on it, so the drive
    /// explains itself the first time it is opened.
    fn format_fresh(&mut self) {
        let seed: [(&str, &[u8]); 7] = [
            ("readme.txt", README),
            ("hello.py", HELLO_PY),
            ("bounce.py", BOUNCE_PY),
            ("keys.py", KEYS_PY),
            ("jiggle.py", JIGGLE_PY),
            ("usbid.py", USBID_PY),
            ("keyboard.py", KEYBOARD_PY),
        ];
        if let Err(e) = badgy_fat::format(msc::disk(), "BADGYOS", crate::usb::volume_id(), &seed) {
            crate::println!("scripts: format failed: {:?}", e);
        }
    }
}

// The files a fresh volume is seeded with.
//
// They live in `samples/` rather than as escaped byte strings so that they can
// be read, edited and -- the point -- executed by a host test:
// `pycon/tests/samples.rs` compiles and runs each of these against a
// no-op host on every `cargo test`. A sample that does not parse is a sample
// the badge would show an error for on first boot.
const README: &[u8] = include_bytes!("../samples/readme.txt");
const HELLO_PY: &[u8] = include_bytes!("../samples/hello.py");
const BOUNCE_PY: &[u8] = include_bytes!("../samples/bounce.py");
const KEYS_PY: &[u8] = include_bytes!("../samples/keys.py");
const JIGGLE_PY: &[u8] = include_bytes!("../samples/jiggle.py");
const USBID_PY: &[u8] = include_bytes!("../samples/usbid.py");
const KEYBOARD_PY: &[u8] = include_bytes!("../samples/keyboard.py");
