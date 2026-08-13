//! An N-key-rollover HID keyboard, on interface 2 of the same composite device.
//!
//! # Why a second HID interface
//!
//! The mouse in [`super::hid`] made the badge something a host's HID stack will
//! bind to; this makes it something a host will *take keystrokes from*. They are
//! deliberately two interfaces rather than one composite report: a boot mouse
//! and a boot keyboard each want their own `bInterfaceProtocol`, and a BIOS or
//! KVM that puts the device in boot protocol expects to find them separately.
//! Scripts reach this through the `key_*` / `type` builtins and read the host's
//! lock LEDs back with `kbd_leds()`.
//!
//! # NKRO, and why it still carries a boot report
//!
//! The report the host reads in *report protocol* is a bitmap: one bit per
//! keycode, so any number of keys can be held at once. That is what "N-key
//! rollover" means and it is what a bitmap gets you for free -- there is no
//! six-key array to overflow.
//!
//! ```text
//!   report protocol (default), 17 bytes:
//!     [0]      modifiers (bit 0 LeftCtrl .. bit 7 RightGUI)
//!     [1..17]  128-bit keycode bitmap; bit k set means usage k is held
//! ```
//!
//! But the interface still claims the boot keyboard subclass, because a host in
//! *boot protocol* (a BIOS menu, some KVMs) ignores the report descriptor
//! entirely and expects the fixed 8-byte boot layout:
//!
//! ```text
//!   boot protocol, 8 bytes:
//!     [0]      modifiers
//!     [1]      reserved (0)
//!     [2..8]   up to six held keycodes, or six 0x01 (ErrorRollOver) if more
//! ```
//!
//! [`PROTOCOL`] tracks which one the host selected and [`send`] packs the
//! current key state into whichever it asked for -- exactly the trick the mouse
//! plays with its boot report, one level more involved because six keys have to
//! be recovered from the bitmap.
//!
//! # Reading the host back
//!
//! A keyboard is the one HID device a host talks *to*: it pushes the Num / Caps
//! / Scroll lock state down as an output report whenever it changes. Most hosts
//! send it as a SET_REPORT on the control pipe rather than to an interrupt OUT
//! endpoint, so there is no OUT endpoint here -- [`super::proto`] catches the
//! control transfer, and [`on_host_leds`] records the byte. That byte is the
//! whole basis of the Caps Lock LED trick the OS-detection path in `runner`
//! plays: toggle a lock key, watch how -- and how fast -- the host echoes it.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use bao1x_hal::usb::driver::*;

/// Interface number. Mass storage is 0, the mouse is 1.
pub const KBD_INTERFACE: u8 = 2;
/// Endpoint address as it appears in the descriptor.
pub const EP_INTR_IN: u8 = 0x83;
/// ...and as the driver names it, the number without the direction bit.
const EP_NUM: u8 = 3;

/// Report size the endpoint advertises. The NKRO report is 17 bytes; 32 leaves
/// it in a single packet at either speed with room to spare.
pub const INTR_MPS: u16 = 32;

// The interrupt endpoint's `bInterval` is the shared HID poll interval that
// `proto::write_config` already selects by speed for the mouse -- 10 ms full
// speed, 8 ms high speed. A keyboard wants nothing finer, so it reuses it
// rather than declaring its own.

/// Bytes in the NKRO keycode bitmap. Sixteen covers usages 0x00..=0x7F, which is
/// every standard key plus the keypad, F13..F24 and the system keys -- the
/// modifiers (0xE0..=0xE7) ride in the separate modifier byte, not the bitmap.
pub const BITMAP_BYTES: usize = 16;
/// Highest keycode the bitmap can express. A code above this has no bit.
pub const MAX_KEYCODE: u8 = (BITMAP_BYTES * 8 - 1) as u8;

/// Bytes in a report-protocol (NKRO) report: modifier byte plus the bitmap.
pub const REPORT_LEN: usize = 1 + BITMAP_BYTES;
/// Bytes in a boot-protocol report: modifiers, a reserved byte, six keycodes.
const BOOT_REPORT_LEN: usize = 8;
/// How many keycodes a boot report can carry before it has to say "too many".
const BOOT_KEYS: usize = 6;

/// Modifier keycodes occupy 0xE0..=0xE7 and map to the bits of the modifier
/// byte rather than the bitmap.
const MOD_MIN: u8 = 0xE0;
const MOD_MAX: u8 = 0xE7;

// ------------------------------------------------------------- class requests
//
// The HID class request codes and descriptor-type numbers are the same for any
// HID interface, so they are shared with the mouse rather than redeclared.

// ------------------------------------------------------------------- LED bits

/// Num Lock's bit in the host's lock-LED output report -- the one the OS probe
/// reads to see whether the host enabled NumLock at enumeration. The full LED
/// bit layout a script sees is documented at `pycon::host::LED_*`.
pub const LED_NUM: u8 = 1 << 0;

// --------------------------------------------------------- report descriptor

/// An NKRO keyboard: eight modifier bits, a five-bit LED output report (so the
/// host has somewhere to push Num/Caps/Scroll lock), three bits of output
/// padding, then a 128-bit input bitmap of held keys.
///
/// The modifier byte and the output report together are byte-for-byte the start
/// of the boot keyboard report, which is what keeps the boot subclass claim
/// honest: a host in boot protocol reads the first byte as modifiers and the
/// rest as a key array, and gets something sensible.
pub const REPORT_DESC: [u8; 57] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0xE0, //   Usage Minimum (Left Control)
    0x29, 0xE7, //   Usage Maximum (Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute) -- the eight modifier bits
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Minimum (Num Lock)
    0x29, 0x05, //   Usage Maximum (Kana)
    0x95, 0x05, //   Report Count (5)
    0x75, 0x01, //   Report Size (1)
    0x91, 0x02, //   Output (Data, Variable, Absolute) -- host-driven lock LEDs
    0x95, 0x01, //   Report Count (1)
    0x75, 0x03, //   Report Size (3)
    0x91, 0x03, //   Output (Constant) -- pad the LED byte out to eight bits
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0x7F, //   Usage Maximum (0x7F)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x80, //   Report Count (128)
    0x81, 0x02, //   Input (Data, Variable, Absolute) -- the NKRO key bitmap
    0xC0, // End Collection
];

// -------------------------------------------------------------- report buffer

/// Where the keyboard report lives, so the controller can DMA it. Slot five of
/// the driver's eight application buffers: mass storage uses 0..=3 and the mouse
/// uses 4, so 5 is the next one free.
const REPORT_BUF_OFFSET: usize = CRG_UDC_APP_BUF_LEN * 5;

const fn report_addr() -> usize {
    bao1x_hal::board::CRG_UDC_MEMBASE + CRG_UDC_APP_BUFOFFSET + REPORT_BUF_OFFSET
}

// The slot has to fall inside the region the driver reserved for application
// buffers, checked here rather than trusted because the arithmetic leans on
// three constants from someone else's crate.
const _: () = assert!(REPORT_BUF_OFFSET + CRG_UDC_APP_BUF_LEN <= CRG_UDC_APP_BUFSIZE);
const _: () = assert!(REPORT_LEN <= CRG_UDC_APP_BUF_LEN);

// ------------------------------------------------------------------ key state
//
// The set of keys held right now, as the source of truth `send` packs into a
// report. It is a `static mut` rather than atomics because it is a small struct
// with an invariant across its fields, and -- like `proto`'s product name and
// `msc`'s disk -- it is only ever touched from the script side of the firmware,
// between `poll()` calls, never from an interrupt or the event handler. There
// are no interrupts and no threads, so there is no borrow to race with.

struct KeyState {
    /// Modifier byte: bit 0 Left Control .. bit 7 Right GUI.
    modifiers: u8,
    /// One bit per keycode, LSB-first within each byte: keycode `k` is bit
    /// `k & 7` of byte `k >> 3`.
    bitmap: [u8; BITMAP_BYTES],
}

static mut KEYS: KeyState = KeyState { modifiers: 0, bitmap: [0; BITMAP_BYTES] };

/// A reference to the key state. Sound under the single-threaded, no-interrupt
/// invariant this whole firmware relies on: the only callers are the script
/// host and `send`, both on the one stack, between polls.
#[allow(clippy::mut_from_ref)]
fn keys() -> &'static mut KeyState {
    // safety: single hart, no interrupts, and this is the only path to `KEYS`.
    unsafe { &mut *core::ptr::addr_of_mut!(KEYS) }
}

// ---------------------------------------------------------------------- state

/// The host has selected a configuration, so the interface is live.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

/// A report is on the transfer ring and has not completed yet. As with the
/// mouse, exactly one is allowed in flight so a script cannot build a backlog
/// the host then replays after the script has stopped.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 0 = boot protocol, 1 = report protocol. Report protocol is the power-on
/// default per the HID spec, and what a host uses unless it says otherwise.
static PROTOCOL: AtomicU8 = AtomicU8::new(1);

/// Whatever the host last passed to SET_IDLE, so GET_IDLE can echo it. It
/// changes no behaviour here -- nothing resends a report on its own.
static IDLE: AtomicU8 = AtomicU8::new(0);

/// The lock LEDs the host last pushed down, as a bitmap of [`LED_NUM`] and
/// friends. This is the readback: the only thing the host tells the keyboard.
static HOST_LEDS: AtomicU8 = AtomicU8::new(0);

/// How many LED output reports the host has sent since the interface was
/// configured. The OS-detection probe watches this counter rather than the LED
/// value, so it can tell "the host answered" from "the host answered with the
/// same bits" -- a toggle that ends where it started still bumps the count.
static LED_EVENTS: AtomicU32 = AtomicU32::new(0);

/// Set by the control handler the instant before it arms an EP0 OUT receive for
/// a lock-LED SET_REPORT, and cleared when that data arrives. It gates the
/// capture in `proto::handle_event` so an unrelated control transfer's status
/// stage is never mistaken for LED data. See [`arm_led_capture`].
static LED_CAPTURE: AtomicBool = AtomicBool::new(false);

// ------------------------------------------------------------------ lifecycle

/// Arm the interrupt endpoint. Called from SET_CONFIGURATION, beside
/// `msc::on_configured` and `hid::on_configured`.
pub fn on_configured(usb: &mut CorigineUsb) {
    usb.ep_enable(EP_NUM, USB_SEND, INTR_MPS, EpType::IntrInbound);
    usb.assign_completion_handler(report_complete, EP_NUM, USB_SEND);

    IN_FLIGHT.store(false, Ordering::SeqCst);
    // Keys do not survive a reconnect: a host that never saw the press must not
    // be told about the release, and must not inherit a phantom hold.
    keys().modifiers = 0;
    keys().bitmap = [0; BITMAP_BYTES];
    PROTOCOL.store(1, Ordering::SeqCst);
    // A fresh enumeration starts the LED history over, so the OS probe measures
    // this host and not the last one.
    HOST_LEDS.store(0, Ordering::SeqCst);
    LED_EVENTS.store(0, Ordering::SeqCst);
    LED_CAPTURE.store(false, Ordering::SeqCst);
    CONFIGURED.store(true, Ordering::SeqCst);
}

/// The host dropped the configuration.
pub fn on_deconfigured() {
    CONFIGURED.store(false, Ordering::SeqCst);
    IN_FLIGHT.store(false, Ordering::SeqCst);
    LED_CAPTURE.store(false, Ordering::SeqCst);
    keys().modifiers = 0;
    keys().bitmap = [0; BITMAP_BYTES];
}

fn report_complete(_usb: &mut CorigineUsb, _addr: usize, _info: u32, _err: u8, _residual: u16) {
    IN_FLIGHT.store(false, Ordering::SeqCst);
}

// ----------------------------------------------------------- host LED readback

/// Arm capture of the next EP0 OUT data stage as a lock-LED report.
///
/// The control handler calls this immediately before staging the receive; the
/// event handler consumes it with [`take_led_capture`] when the data lands.
pub fn arm_led_capture() { LED_CAPTURE.store(true, Ordering::SeqCst); }

/// Was an LED capture armed? Clears the flag, so exactly one EP0 OUT completion
/// is treated as LED data per SET_REPORT.
pub fn take_led_capture() -> bool { LED_CAPTURE.swap(false, Ordering::SeqCst) }

/// Record a lock-LED byte the host pushed down. Called from the event handler
/// once the SET_REPORT data has been DMA'd in.
pub fn on_host_leds(byte: u8) {
    HOST_LEDS.store(byte, Ordering::SeqCst);
    LED_EVENTS.fetch_add(1, Ordering::SeqCst);
}

/// The lock LEDs the host last reported, as a bitmap of [`LED_NUM`] etc.
pub fn host_leds() -> u8 { HOST_LEDS.load(Ordering::SeqCst) }

/// How many LED reports the host has sent since configuration. The OS probe
/// snapshots this, toggles a lock key, and waits for it to move.
pub fn led_events() -> u32 { LED_EVENTS.load(Ordering::SeqCst) }

// --------------------------------------------------------------- key edits
//
// These only edit the shared key state; they do not send. A caller that changes
// several keys and then calls `send` once gets one report carrying all of them,
// which is what makes a chord (Ctrl+Alt+Del) a single event to the host rather
// than three.

/// Press `code`. A modifier keycode (0xE0..=0xE7) sets its modifier bit; any
/// other in range sets its bitmap bit. Returns false for a code the report
/// cannot express, so a caller can tell the script rather than silently drop it.
pub fn key_down(code: u8) -> bool {
    if (MOD_MIN..=MOD_MAX).contains(&code) {
        keys().modifiers |= 1 << (code - MOD_MIN);
        true
    } else if code <= MAX_KEYCODE {
        keys().bitmap[(code >> 3) as usize] |= 1 << (code & 7);
        true
    } else {
        false
    }
}

/// Release `code`. Unknown or out-of-range codes are a no-op that still reports
/// success: releasing a key that was never down is harmless, and refusing it
/// would only complicate the caller.
pub fn key_up(code: u8) -> bool {
    if (MOD_MIN..=MOD_MAX).contains(&code) {
        keys().modifiers &= !(1 << (code - MOD_MIN));
    } else if code <= MAX_KEYCODE {
        keys().bitmap[(code >> 3) as usize] &= !(1 << (code & 7));
    }
    true
}

/// Replace the modifier byte wholesale. Used to hold Shift for a capital or a
/// chord's Ctrl/Alt without disturbing the keys already down.
pub fn set_modifiers(mask: u8) { keys().modifiers = mask; }

/// Let go of everything -- every key and every modifier. The state to send when
/// a script ends or wants a clean slate.
pub fn release_all() {
    keys().modifiers = 0;
    keys().bitmap = [0; BITMAP_BYTES];
}

/// Is any key or modifier currently held?
pub fn any_down() -> bool {
    keys().modifiers != 0 || keys().bitmap.iter().any(|&b| b != 0)
}

// --------------------------------------------------------------------- sending

/// Is there a host that would receive a report?
pub fn is_ready() -> bool { CONFIGURED.load(Ordering::SeqCst) }

/// Is the endpoint free to take another report right now?
pub fn is_idle() -> bool { !IN_FLIGHT.load(Ordering::SeqCst) }

/// 0 = boot, 1 = report.
pub fn protocol() -> u8 { PROTOCOL.load(Ordering::SeqCst) }

pub fn set_protocol(p: u8) { PROTOCOL.store(if p == 0 { 0 } else { 1 }, Ordering::SeqCst); }

pub fn set_idle(duration: u8) { IDLE.store(duration, Ordering::SeqCst); }

pub fn idle() -> u8 { IDLE.load(Ordering::SeqCst) }

/// Build the report for the protocol currently in force into `out`, returning
/// how many bytes it occupies. Shared by [`send`] and by GET_REPORT so the two
/// can never disagree about what the current state looks like on the wire.
fn build_report(out: &mut [u8; REPORT_LEN]) -> usize {
    let k = keys();
    if PROTOCOL.load(Ordering::SeqCst) == 0 {
        // Boot protocol: modifiers, a reserved zero, then up to six keycodes
        // recovered from the bitmap in ascending order. More than six held is
        // the rollover case the boot report cannot represent, so every slot
        // becomes 0x01 (ErrorRollOver), which is exactly what the spec asks a
        // boot keyboard to send when it runs out of room.
        out[..BOOT_REPORT_LEN].fill(0);
        out[0] = k.modifiers;
        let mut slot = 0;
        let mut overflow = false;
        'scan: for (byte, &bits) in k.bitmap.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            for bit in 0..8 {
                if bits & (1 << bit) != 0 {
                    if slot >= BOOT_KEYS {
                        overflow = true;
                        break 'scan;
                    }
                    out[2 + slot] = (byte * 8 + bit) as u8;
                    slot += 1;
                }
            }
        }
        if overflow {
            out[2..2 + BOOT_KEYS].fill(0x01);
        }
        BOOT_REPORT_LEN
    } else {
        // Report protocol: the modifier byte then the bitmap, verbatim. This is
        // the whole point of the interface -- no key can crowd out another.
        out[0] = k.modifiers;
        out[1..REPORT_LEN].copy_from_slice(&k.bitmap);
        REPORT_LEN
    }
}

/// Queue a report carrying the current key state. Returns false if the interface
/// is down or the previous report has not been collected yet -- neither is an
/// error, just "not now", and the caller decides whether to wait and retry.
///
/// # Safety of the call into the driver
///
/// As with the mouse, this reaches the global controller through
/// [`super::with_usb`], sound only because nothing here is reentrant: no
/// interrupts, no threads, and it is called from the script host between polls,
/// never from inside a completion handler.
pub fn send() -> bool {
    if !is_ready() || !is_idle() {
        return false;
    }
    let mut report = [0u8; REPORT_LEN];
    let len = build_report(&mut report);

    // safety: `report_addr()` is a 512-byte application slot inside the IFRAM
    // window the driver reserved, checked at compile time above, and this module
    // is its only user.
    let buf = unsafe { core::slice::from_raw_parts_mut(report_addr() as *mut u8, REPORT_LEN) };
    buf[..len].copy_from_slice(&report[..len]);

    IN_FLIGHT.store(true, Ordering::SeqCst);
    let queued = super::with_usb(|u| u.bulk_xfer(EP_NUM, USB_SEND, report_addr(), len, 0, 0));
    if queued.is_none() {
        // The controller went away between the readiness check and here. Do not
        // leave the flag set, or every later report is refused.
        IN_FLIGHT.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

/// The current report, for GET_REPORT, in the protocol in force. Returns how
/// many bytes of `out` it filled.
pub fn current_report(out: &mut [u8; REPORT_LEN]) -> usize { build_report(out) }
