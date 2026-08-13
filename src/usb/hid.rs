//! A boot-protocol HID mouse, on interface 1 of the same composite device.
//!
//! # Why the firmware and not just a script
//!
//! "Jiggle the mouse" is a script-shaped idea, but there is nothing for a
//! script to jiggle: the device this firmware builds is a mass-storage device
//! and nothing else, so the only way a script can move a host's pointer is if
//! the device grows an interface that a host's HID driver will bind to. That is
//! this module. Scripts reach it through three `Host` methods and the
//! `mouse_move` / `mouse_buttons` / `mouse_click` builtins.
//!
//! # Shape
//!
//! One interrupt IN endpoint, EP2 (`0x82`), carrying a four-byte report:
//!
//! ```text
//!   [0] buttons, bit 0 left, bit 1 right, bit 2 middle
//!   [1] dx, signed, -127..127
//!   [2] dy, signed, positive is down
//!   [3] wheel, signed, positive is away from the user
//! ```
//!
//! The first three bytes are exactly the HID boot mouse report, which is why
//! the interface claims boot subclass: a host that puts us in boot protocol
//! (BIOS menus, some KVMs) gets a report it understands by simply truncating,
//! and no second descriptor is needed. [`PROTOCOL`] tracks which one is in
//! force and [`send`] shortens the transfer accordingly.
//!
//! # What it cannot do
//!
//! Wake a *suspended* host. That needs remote wakeup, which needs the device to
//! drive resume signalling, and this configuration declares itself self-powered
//! with no remote wakeup (accurately -- the badge has its own battery). What it
//! does do is keep an awake host from deciding it is idle, which is the whole
//! job of a jiggler.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use bao1x_hal::usb::driver::*;

/// Interface number. Mass storage is 0; see `proto::MSC_INTERFACE`.
pub const HID_INTERFACE: u8 = 1;
/// Endpoint address as it appears in the descriptor.
pub const EP_INTR_IN: u8 = 0x82;
/// ...and as the driver names it, which is the number without the direction bit.
const EP_NUM: u8 = 2;

/// Eight bytes for a four-byte report. The report never grows, but eight is the
/// classic boot-mouse packet size and costs nothing at either speed.
pub const INTR_MPS: u16 = 8;

/// `bInterval` at full speed, in milliseconds.
pub const FS_INTR_INTERVAL: u8 = 10;
/// `bInterval` at high speed, where the field is an exponent: the host polls
/// every `2^(n-1)` microframes, so 7 is 64 microframes, i.e. 8 ms. Close enough
/// to the full-speed figure that a script behaves the same at either speed.
pub const HS_INTR_INTERVAL: u8 = 7;

/// Bytes in a report protocol report.
pub const REPORT_LEN: usize = 4;
/// Bytes in a boot protocol report -- the same layout, without the wheel.
const BOOT_REPORT_LEN: usize = 3;

/// Button bits, mirrored into `pycon` as `MOUSE_LEFT` and friends.
pub const BUTTON_MASK: u8 = 0x07;

// ------------------------------------------------------------- class requests

pub const HID_REQ_GET_REPORT: u8 = 0x01;
pub const HID_REQ_GET_IDLE: u8 = 0x02;
pub const HID_REQ_GET_PROTOCOL: u8 = 0x03;
pub const HID_REQ_SET_REPORT: u8 = 0x09;
pub const HID_REQ_SET_IDLE: u8 = 0x0A;
pub const HID_REQ_SET_PROTOCOL: u8 = 0x0B;

/// Descriptor types that arrive as ordinary GET_DESCRIPTOR requests aimed at
/// the interface rather than the device.
pub const USB_DT_HID: u8 = 0x21;
pub const USB_DT_HID_REPORT: u8 = 0x22;

// --------------------------------------------------------- report descriptor

/// The stock mouse report descriptor: three buttons, five bits of padding, then
/// three signed bytes of X, Y and wheel.
///
/// Deliberately unadventurous. Every host has parsed this exact shape since
/// 1998, and the first three bytes it describes are byte-identical to the boot
/// mouse report, which is what makes the boot subclass claim honest.
pub const REPORT_DESC: [u8; 52] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (Button 1)
    0x29, 0x03, //     Usage Maximum (Button 3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x03, //     Input (Constant) -- pad the button byte out to eight bits
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x03, //     Report Count (3)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xC0, //   End Collection
    0xC0, // End Collection
];

// -------------------------------------------------------------- report buffer

/// Where the report lives, so the controller can DMA it.
///
/// The driver carves `CRG_UDC_APP_BUFSIZE` (eight 512-byte slots) out of its
/// IFRAM window and hands them out by endpoint. `msc.rs` uses the first four
/// -- CBW, CSW, and a 1 KiB EP1 IN staging buffer -- so slot four is free.
///
/// It is nominally the slot `get_app_buf_ptr` computes for EP3 IN, which is the
/// keyboard's endpoint (`kbd.rs`, using slot five) -- but nothing here calls
/// `get_app_buf_ptr`: both HID reports are queued with an explicit address
/// through `bulk_xfer`, so that assignment is never made and slots four and
/// five belong to the mouse and keyboard alone. Using UDC memory rather than
/// IFRAM1 keeps the report inside the window `ptr_in_udc_window` already
/// vouches for.
const REPORT_BUF_OFFSET: usize = CRG_UDC_APP_BUF_LEN * 4;

const fn report_addr() -> usize {
    bao1x_hal::board::CRG_UDC_MEMBASE + CRG_UDC_APP_BUFOFFSET + REPORT_BUF_OFFSET
}

// The slot has to be inside the region the driver reserved, or the DMA would
// land on whatever follows it. Checked here rather than trusted, because the
// arithmetic depends on three constants in someone else's crate.
const _: () = assert!(REPORT_BUF_OFFSET + CRG_UDC_APP_BUF_LEN <= CRG_UDC_APP_BUFSIZE);

// ---------------------------------------------------------------------- state

/// The host has selected a configuration, so the interface is live.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

/// A report is on the transfer ring and has not completed yet.
///
/// Reports are sent one at a time. The ring would hold 64, but queuing ahead
/// would mean a script that calls `mouse_move` in a tight loop builds a backlog
/// the host then plays out over the following second -- the pointer would keep
/// moving after the script stopped. One in flight makes `send` return false
/// under back-pressure instead, and the caller can decide what to do about it.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 0 = boot protocol, 1 = report protocol.
///
/// Report protocol is the power-on default per the HID spec. Linux and Windows
/// both set it explicitly anyway.
static PROTOCOL: AtomicU8 = AtomicU8::new(1);

/// Whatever the host last passed to SET_IDLE, so GET_IDLE can echo it.
///
/// It has no other effect: idle-rate repeat means "resend the last report every
/// N*4 ms even if nothing changed", and nothing here sends a report the script
/// did not ask for. Zero -- report on change only -- is the value every mouse
/// driver actually sets.
static IDLE: AtomicU8 = AtomicU8::new(0);

/// The last report sent, packed little-endian, for GET_REPORT to hand back.
static LAST: AtomicU32 = AtomicU32::new(0);

// ------------------------------------------------------------------ lifecycle

/// Arm the interrupt endpoint. Called from SET_CONFIGURATION, beside
/// `msc::on_configured`.
pub fn on_configured(usb: &mut CorigineUsb) {
    usb.ep_enable(EP_NUM, USB_SEND, INTR_MPS, EpType::IntrInbound);
    usb.assign_completion_handler(report_complete, EP_NUM, USB_SEND);

    // A configuration change throws away anything the ring was holding, so a
    // stale in-flight flag would wedge the endpoint until reboot.
    IN_FLIGHT.store(false, Ordering::SeqCst);
    // Buttons do not survive a reconnect: a host that never saw the press must
    // not be told about the release.
    LAST.store(0, Ordering::SeqCst);
    PROTOCOL.store(1, Ordering::SeqCst);
    CONFIGURED.store(true, Ordering::SeqCst);
}

/// The host dropped the configuration.
pub fn on_deconfigured() {
    CONFIGURED.store(false, Ordering::SeqCst);
    IN_FLIGHT.store(false, Ordering::SeqCst);
    LAST.store(0, Ordering::SeqCst);
}

fn report_complete(_usb: &mut CorigineUsb, _addr: usize, _info: u32, _err: u8, _residual: u16) {
    IN_FLIGHT.store(false, Ordering::SeqCst);
}

// --------------------------------------------------------------------- sending

/// Is there a host that would receive a report?
pub fn is_ready() -> bool { CONFIGURED.load(Ordering::SeqCst) }

/// Is the endpoint free to take another report right now?
pub fn is_idle() -> bool { !IN_FLIGHT.load(Ordering::SeqCst) }

/// Queue one report. Returns false if the interface is down or the previous
/// report has not been collected yet -- neither is an error, just "not now".
///
/// # Safety of the call into the driver
///
/// This reaches for the global controller through [`super::with_usb`], which is
/// sound only because nothing here is reentrant: the firmware has no interrupts
/// and no threads, and this is called from the script host between `poll()`
/// calls, never from inside a completion handler.
pub fn send(buttons: u8, dx: i8, dy: i8, wheel: i8) -> bool {
    if !is_ready() || !is_idle() {
        return false;
    }
    let report = [buttons & BUTTON_MASK, dx as u8, dy as u8, wheel as u8];
    LAST.store(u32::from_le_bytes(report), Ordering::SeqCst);

    // safety: `report_addr()` is a 512-byte slot inside the IFRAM window the
    // driver reserved for application buffers, checked at compile time above,
    // and this module is its only user.
    let buf = unsafe { core::slice::from_raw_parts_mut(report_addr() as *mut u8, REPORT_LEN) };
    buf.copy_from_slice(&report);

    let len = if PROTOCOL.load(Ordering::SeqCst) == 0 { BOOT_REPORT_LEN } else { REPORT_LEN };
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

/// The last report, for GET_REPORT.
pub fn last_report() -> [u8; REPORT_LEN] { LAST.load(Ordering::SeqCst).to_le_bytes() }

/// Length of a report in the protocol currently in force.
pub fn report_len() -> usize {
    if PROTOCOL.load(Ordering::SeqCst) == 0 { BOOT_REPORT_LEN } else { REPORT_LEN }
}

pub fn set_protocol(p: u8) { PROTOCOL.store(if p == 0 { 0 } else { 1 }, Ordering::SeqCst); }

pub fn protocol() -> u8 { PROTOCOL.load(Ordering::SeqCst) }

pub fn set_idle(duration: u8) { IDLE.store(duration, Ordering::SeqCst); }

pub fn idle() -> u8 { IDLE.load(Ordering::SeqCst) }
