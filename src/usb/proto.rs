//! USB descriptors and the EP0 control transfer handler.
//!
//! Ported from `xous-core/bao1x-boot/boot1/src/platform/bao1x/usb/{mod,driver}.rs`,
//! reduced to mass storage and then grown a HID mouse. boot1 enumerates as a
//! composite CDC + MSC device; dropping the CDC half removed two interfaces,
//! three endpoints, the interface-association descriptor and eleven
//! CDC-specific class requests -- and, incidentally, three latent bugs in the
//! original:
//!
//! * `EP1_IN_BUF` and boot1's CDC receive buffer resolve to the same IFRAM address, so a bulk IN reply can be
//!   overwritten by serial traffic;
//! * its Bulk-Only Reset handler compares `wIndex` against interface 0, but in the composite layout mass
//!   storage is interface 2, so the reset silently stalls instead of recovering the endpoints;
//! * `DISK_BUSY` is shared between the two functions.
//!
//! The layout here is three independent single-interface functions:
//!
//! | iface | class | endpoints | what it is |
//! |---|---|---|---|
//! | 0 | 08/06/50 | `0x01` OUT, `0x81` IN (bulk) | the script drive, `msc.rs` |
//! | 1 | 03/01/02 | `0x82` IN (interrupt) | the mouse, `hid.rs` |
//! | 2 | 03/01/01 | `0x83` IN (interrupt) | the keyboard, `kbd.rs` |
//!
//! No interface association descriptor: an IAD groups several interfaces into
//! one function, and none of these spans more than one. Each keeps its own
//! class requests, routed by `wIndex` rather than shared -- which is the bug
//! boot1 has. The keyboard's lock LEDs are the one thing a host sends *down*,
//! and it arrives as a SET_REPORT on this control pipe, caught in
//! [`hid_kbd_class_request`] and [`handle_event`]'s EP0 OUT path.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use bao1x_hal::usb::driver::*;

use super::hid::{self, HID_INTERFACE};
use super::kbd::{self, KBD_INTERFACE};
use super::msc;

// ---------------------------------------------------------------- identity
//
// Vendor id, product id and product name used to be `const`s: this firmware was
// exactly one device. They are runtime state now, because a script can change
// them (`usb_id()` / `usb_name()`) and have the badge re-present itself to the
// host as whatever it likes. The descriptors below read these functions rather
// than constants, and `super::reattach` is what makes a change visible -- the
// host only reads the device descriptor at enumeration, so nothing short of a
// re-enumeration will move a running host off the identity it already latched.

/// Openmoko's shared vendor id, which Baochip uses for this family. The default
/// vendor, and what `usb_id(pid)` keeps when a script sets only the product id.
pub const DEFAULT_VENDOR_ID: u16 = 0x1d50;
/// Deliberately *not* `0x6196`. That is boot1's bootloader drive, and
/// `tools/badgeflash.py` finds the flashable volume by matching exactly
/// `1d50:6196` -- if this drive answered to it, a flash could be aimed at the
/// script volume. `0x6197` and `0x6198` are also taken (dabao and baosec Xous).
pub const DEFAULT_PRODUCT_ID: u16 = 0x6199;

const MANUFACTURER: &str = "Baochip";
/// Shown as the device name until a script sets one with `usb_name()`.
const DEFAULT_PRODUCT: &str = "BadgyOS";

/// Vendor id in the high half, product id in the low half. Packed into one word
/// so the pair moves atomically: a host that sampled a half-applied identity
/// would bind its driver against a vendor and product that never went together.
static IDENTITY: AtomicU32 = AtomicU32::new(((DEFAULT_VENDOR_ID as u32) << 16) | DEFAULT_PRODUCT_ID as u32);

pub fn vendor_id() -> u16 { (IDENTITY.load(Ordering::SeqCst) >> 16) as u16 }
pub fn product_id() -> u16 { IDENTITY.load(Ordering::SeqCst) as u16 }

/// Set the vendor and product id used from the next enumeration on. Returns
/// false -- and changes nothing -- for the one pair that has to stay unique:
/// boot1's bootloader drive at `1d50:6196`, which `tools/badgeflash.py` locates
/// by an exact match, so a running badge answering to it could catch a flash
/// meant for the bootloader.
///
/// This does not itself re-enumerate; `super::set_identity` pairs it with a
/// [`super::reattach`] so a host sees the change.
pub fn set_identity(vid: u16, pid: u16) -> bool {
    if vid == 0x1d50 && pid == 0x6196 {
        return false;
    }
    IDENTITY.store(((vid as u32) << 16) | pid as u32, Ordering::SeqCst);
    true
}

/// Longest product string a script can set. A USB string descriptor tops out at
/// 126 UTF-16 units; this is well under that and keeps the buffer small enough
/// to stay out of the image (see below).
const PRODUCT_NAME_CAP: usize = 32;

/// The script-set product name, or empty for "use the default".
///
/// Zero-initialized on purpose: a zero-filled `static mut` lands in `.bss`,
/// which is NOBITS, so it costs nothing in the flashed image and needs no
/// entries in the image builder's 40-slot poke table -- exactly the reasoning
/// the RAM disk in `msc.rs` documents at length.
static mut PRODUCT_NAME: [u8; PRODUCT_NAME_CAP] = [0; PRODUCT_NAME_CAP];
static PRODUCT_NAME_LEN: AtomicUsize = AtomicUsize::new(0);

/// The product name in force, as raw bytes for the string-descriptor writer.
fn product_name() -> &'static [u8] {
    let len = PRODUCT_NAME_LEN.load(Ordering::SeqCst);
    if len == 0 {
        DEFAULT_PRODUCT.as_bytes()
    } else {
        // safety: single-threaded, no interrupts; `len` bytes were written by
        // `set_product_name` and `len <= PRODUCT_NAME_CAP` by construction. A
        // raw slice built straight from the pointer, so no `&` ever touches the
        // `static mut` itself.
        unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(PRODUCT_NAME) as *const u8, len) }
    }
}

/// Replace the product name. An empty string restores [`DEFAULT_PRODUCT`].
///
/// Paired with a [`super::reattach`] by `super::set_product_name`; on its own it
/// only stages the bytes.
pub fn set_product_name(name: &str) {
    let mut n = name.len().min(PRODUCT_NAME_CAP);
    // Truncate on a character boundary. The descriptor writer widens each byte
    // to a UTF-16 unit rather than decoding, so a clipped multibyte character
    // would only garble the on-screen name -- but trimming cleanly is free.
    while n > 0 && !name.is_char_boundary(n) {
        n -= 1;
    }
    // safety: single-threaded, no interrupts, and `n <= PRODUCT_NAME_CAP`.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PRODUCT_NAME) };
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    PRODUCT_NAME_LEN.store(n, Ordering::SeqCst);
}

// ------------------------------------------------------------- descriptors

const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_CONFIG: u8 = 0x02;
const USB_DT_STRING: u8 = 0x03;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_ENDPOINT: u8 = 0x05;
const USB_DT_DEVICE_QUALIFIER: u8 = 0x06;
const USB_DT_OTHER_SPEED_CONFIG: u8 = 0x07;
const USB_DT_BOS: u8 = 0x0f;
const USB_DT_DEVICE_CAPABILITY: u8 = 0x10;
const USB_CAP_TYPE_EXT: u8 = 0x02;

const USB_TYPE_MASK: u8 = 0x03 << 5;
const USB_TYPE_STANDARD: u8 = 0x00 << 5;
const USB_TYPE_CLASS: u8 = 0x01 << 5;

const USB_RECIP_DEVICE: u8 = 0x00;
const USB_RECIP_INTERFACE: u8 = 0x01;
const USB_RECIP_ENDPOINT: u8 = 0x02;

const USB_REQ_GET_STATUS: u8 = 0x00;
const USB_REQ_CLEAR_FEATURE: u8 = 0x01;
const USB_REQ_SET_FEATURE: u8 = 0x03;
const USB_REQ_SET_ADDRESS: u8 = 0x05;
const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQ_GET_CONFIGURATION: u8 = 0x08;
const USB_REQ_SET_CONFIGURATION: u8 = 0x09;
const USB_REQ_GET_INTERFACE: u8 = 0x0A;
const USB_REQ_SET_INTERFACE: u8 = 0x0B;
const USB_REQ_SET_SEL: u8 = 0x30;
const USB_REQ_SET_ISOCH_DELAY: u8 = 0x31;

/// Bulk-Only Transport class requests.
const BOT_REQ_GET_MAX_LUN: u8 = 0xFE;
const BOT_REQ_RESET: u8 = 0xFF;

/// The mass-storage interface. The mouse is `hid::HID_INTERFACE`, which is 1.
pub const MSC_INTERFACE: u8 = 0;
pub const EP_BULK_IN: u8 = 0x81;
pub const EP_BULK_OUT: u8 = 0x01;
pub const FS_BULK_MPS: usize = 64;
pub const HS_BULK_MPS: usize = 512;

/// How many interfaces the one configuration has: mass storage, mouse, keyboard.
const NUM_INTERFACES: u8 = 3;

/// Everything below is written into a byte buffer, so field order and the
/// absence of padding are part of the wire format, not an optimization.
macro_rules! as_bytes {
    ($t:ty) => {
        impl AsRef<[u8]> for $t {
            fn as_ref(&self) -> &[u8] {
                // safety: `repr(C, packed)` of integer fields only, so every
                // byte of the struct is initialized and this is a plain
                // reinterpretation of it.
                unsafe { core::slice::from_raw_parts(self as *const $t as *const u8, size_of::<$t>()) }
            }
        }
    };
}

#[repr(C, packed)]
struct DeviceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    bcd_usb: u16,
    b_device_class: u8,
    b_device_sub_class: u8,
    b_device_protocol: u8,
    b_max_packet_size0: u8,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    i_manufacturer: u8,
    i_product: u8,
    i_serial_number: u8,
    b_num_configurations: u8,
}
as_bytes!(DeviceDescriptor);

impl DeviceDescriptor {
    fn new() -> Self {
        Self {
            b_length: size_of::<Self>() as u8,
            b_descriptor_type: USB_DT_DEVICE,
            bcd_usb: 0x0200,
            // Class zero at the device level: this is a composite device and
            // each interface declares its own class. Naming one of them here
            // would tell a host the whole device is that class.
            b_device_class: 0,
            b_device_sub_class: 0,
            b_device_protocol: 0,
            b_max_packet_size0: 64,
            id_vendor: vendor_id(),
            id_product: product_id(),
            bcd_device: 0x0100,
            i_manufacturer: 1,
            i_product: 2,
            i_serial_number: 3,
            b_num_configurations: 1,
        }
    }
}

#[repr(C, packed)]
struct QualifierDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    bcd_usb: u16,
    b_device_class: u8,
    b_device_sub_class: u8,
    b_device_protocol: u8,
    b_max_packet_size0: u8,
    b_num_configurations: u8,
    b_reserved: u8,
}
as_bytes!(QualifierDescriptor);

#[repr(C, packed)]
struct ConfigDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    w_total_length: u16,
    b_num_interfaces: u8,
    b_configuration_value: u8,
    i_configuration: u8,
    bm_attributes: u8,
    b_max_power: u8,
}
as_bytes!(ConfigDescriptor);

#[repr(C, packed)]
struct InterfaceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_interface_number: u8,
    b_alternate_setting: u8,
    b_num_endpoints: u8,
    b_interface_class: u8,
    b_interface_sub_class: u8,
    b_interface_protocol: u8,
    i_interface: u8,
}
as_bytes!(InterfaceDescriptor);

#[repr(C, packed)]
struct EndpointDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_endpoint_address: u8,
    bm_attributes: u8,
    w_max_packet_size: u16,
    b_interval: u8,
}
as_bytes!(EndpointDescriptor);

/// The HID class descriptor, which sits between the interface and its endpoint
/// and says how long the report descriptor is.
///
/// Only one subordinate descriptor is declared, so the trailing
/// `bDescriptorType`/`wDescriptorLength` pair appears exactly once and the
/// whole thing is a fixed nine bytes.
#[repr(C, packed)]
struct HidDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    bcd_hid: u16,
    b_country_code: u8,
    b_num_descriptors: u8,
    b_report_descriptor_type: u8,
    w_report_descriptor_length: u16,
}
as_bytes!(HidDescriptor);

impl HidDescriptor {
    /// `report_desc_len` is the size of the interface's report descriptor -- the
    /// mouse's and the keyboard's differ, so this cannot be a constant.
    fn new(report_desc_len: u16) -> Self {
        Self {
            b_length: size_of::<Self>() as u8,
            b_descriptor_type: hid::USB_DT_HID,
            bcd_hid: 0x0111,
            // Not localized. This field is for keyboards with country-specific
            // key legends; neither interface claims one, so a host uses whatever
            // layout the OS is set to -- which is why `type` maps US-ASCII.
            b_country_code: 0,
            b_num_descriptors: 1,
            b_report_descriptor_type: hid::USB_DT_HID_REPORT,
            w_report_descriptor_length: report_desc_len,
        }
    }
}

#[repr(C, packed)]
struct BosDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    w_total_length: u16,
    b_num_device_caps: u8,
}
as_bytes!(BosDescriptor);

#[repr(C, packed)]
struct ExtCapDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_dev_capability_type: u8,
    bm_attributes: u32,
}
as_bytes!(ExtCapDescriptor);

/// Length of the configuration bundle: config, then mass storage with its two
/// bulk endpoints, then the mouse and the keyboard, each an interface with its
/// HID class descriptor and one interrupt endpoint.
const CONFIG_TOTAL_LEN: usize = size_of::<ConfigDescriptor>()
    + size_of::<InterfaceDescriptor>()
    + 2 * size_of::<EndpointDescriptor>()
    + 2 * (size_of::<InterfaceDescriptor>() + size_of::<HidDescriptor>() + size_of::<EndpointDescriptor>());

/// Serialize the configuration bundle for one speed.
///
/// `bulk_mps` and `intr_interval` are the only fields that differ between full
/// and high speed, which is what lets the same function serve both
/// `GET_DESCRIPTOR(CONFIG)` and `GET_DESCRIPTOR(OTHER_SPEED_CONFIG)`.
fn write_config(buf: &mut [u8], desc_type: u8, bulk_mps: u16, intr_interval: u8) -> usize {
    let config = ConfigDescriptor {
        b_length: size_of::<ConfigDescriptor>() as u8,
        b_descriptor_type: desc_type,
        w_total_length: CONFIG_TOTAL_LEN as u16,
        b_num_interfaces: NUM_INTERFACES,
        b_configuration_value: 1,
        i_configuration: 0,
        // Self powered, no remote wakeup. The badge has its own battery, and
        // claiming bus power we may not draw would be a lie a host can act on.
        //
        // No remote wakeup is also why the mouse cannot wake a *suspended*
        // host, only keep an awake one from going idle. Claiming the bit
        // without driving resume signalling would be the same kind of lie.
        bm_attributes: 0xC0,
        b_max_power: 250,
    };
    let msc_interface = InterfaceDescriptor {
        b_length: size_of::<InterfaceDescriptor>() as u8,
        b_descriptor_type: USB_DT_INTERFACE,
        b_interface_number: MSC_INTERFACE,
        b_alternate_setting: 0,
        b_num_endpoints: 2,
        b_interface_class: 0x08,     // mass storage
        b_interface_sub_class: 0x06, // SCSI transparent command set
        b_interface_protocol: 0x50,  // Bulk-Only Transport
        i_interface: 0,
    };
    let bulk_ep = |addr: u8| EndpointDescriptor {
        b_length: size_of::<EndpointDescriptor>() as u8,
        b_descriptor_type: USB_DT_ENDPOINT,
        b_endpoint_address: addr,
        bm_attributes: 0x02, // bulk
        w_max_packet_size: bulk_mps,
        b_interval: 0,
    };
    let hid_interface = InterfaceDescriptor {
        b_length: size_of::<InterfaceDescriptor>() as u8,
        b_descriptor_type: USB_DT_INTERFACE,
        b_interface_number: HID_INTERFACE,
        b_alternate_setting: 0,
        b_num_endpoints: 1,
        b_interface_class: 0x03,     // HID
        b_interface_sub_class: 0x01, // boot interface
        b_interface_protocol: 0x02,  // mouse
        i_interface: 0,
    };
    let hid_desc = HidDescriptor::new(hid::REPORT_DESC.len() as u16);
    let intr_ep = EndpointDescriptor {
        b_length: size_of::<EndpointDescriptor>() as u8,
        b_descriptor_type: USB_DT_ENDPOINT,
        b_endpoint_address: hid::EP_INTR_IN,
        bm_attributes: 0x03, // interrupt
        w_max_packet_size: hid::INTR_MPS,
        b_interval: intr_interval,
    };
    let kbd_interface = InterfaceDescriptor {
        b_length: size_of::<InterfaceDescriptor>() as u8,
        b_descriptor_type: USB_DT_INTERFACE,
        b_interface_number: KBD_INTERFACE,
        b_alternate_setting: 0,
        b_num_endpoints: 1,
        b_interface_class: 0x03,     // HID
        b_interface_sub_class: 0x01, // boot interface
        b_interface_protocol: 0x01,  // keyboard
        i_interface: 0,
    };
    let kbd_desc = HidDescriptor::new(kbd::REPORT_DESC.len() as u16);
    let kbd_intr_ep = EndpointDescriptor {
        b_length: size_of::<EndpointDescriptor>() as u8,
        b_descriptor_type: USB_DT_ENDPOINT,
        b_endpoint_address: kbd::EP_INTR_IN,
        bm_attributes: 0x03, // interrupt
        w_max_packet_size: kbd::INTR_MPS,
        b_interval: intr_interval,
    };

    let ep_in = bulk_ep(EP_BULK_IN);
    let ep_out = bulk_ep(EP_BULK_OUT);

    let parts: [&[u8]; 10] = [
        config.as_ref(),
        msc_interface.as_ref(),
        ep_in.as_ref(),
        ep_out.as_ref(),
        hid_interface.as_ref(),
        hid_desc.as_ref(),
        intr_ep.as_ref(),
        kbd_interface.as_ref(),
        kbd_desc.as_ref(),
        kbd_intr_ep.as_ref(),
    ];
    let mut idx = 0;
    for p in parts {
        buf[idx..idx + p.len()].copy_from_slice(p);
        idx += p.len();
    }
    debug_assert_eq!(idx, CONFIG_TOTAL_LEN);
    idx
}

// ------------------------------------------------------------------ EP0

#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct CtrlRequest {
    b_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
}

impl CtrlRequest {
    fn from_bytes(raw: &[u8; 8]) -> Self {
        Self {
            b_request_type: raw[0],
            b_request: raw[1],
            w_value: u16::from_le_bytes([raw[2], raw[3]]),
            w_index: u16::from_le_bytes([raw[4], raw[5]]),
            w_length: u16::from_le_bytes([raw[6], raw[7]]),
        }
    }
}

/// A mutable view of the control endpoint's staging buffer.
///
/// Returns `None` rather than asserting if the pointer is somewhere it should
/// not be. It never should be -- the driver allocated it inside the UDC's own
/// IFRAM window -- but an assertion here would panic, and a panic on this
/// device prints and spins until someone pulls the power. Dropping one control
/// transfer costs a retry; hanging costs the badge.
///
/// # Safety
///
/// The pointer comes from the hardware-facing driver, so it is re-checked
/// against the windows the driver was given.
fn ep0_buf(this: &CorigineUsb) -> Option<&'static mut [u8]> {
    let p = this.ep0_buf.load(Ordering::SeqCst) as usize;
    if !super::ptr_in_udc_window(p, CRG_UDC_EP0_REQBUFSIZE) {
        crate::println!("USB: EP0 buffer at {:#x} is outside the UDC window", p);
        return None;
    }
    // safety: bounds-checked above; this is the only view of that buffer.
    Some(unsafe { core::slice::from_raw_parts_mut(p as *mut u8, CRG_UDC_EP0_REQBUFSIZE) })
}

fn get_status(this: &mut CorigineUsb, request_type: u8, index: u16) {
    let Some(buf) = ep0_buf(this) else { return };
    let recipient = request_type & 0x1f;
    let ep_num = (index & 0x7f) as u8;
    let ep_dir = if index & 0x80 != 0 { USB_SEND } else { USB_RECV };

    buf[0] = 0;
    buf[1] = 0;
    match recipient {
        // Bit 0 of the device status is "self powered", which matches the
        // configuration descriptor's bmAttributes.
        USB_RECIP_DEVICE => buf[0] = 1,
        USB_RECIP_INTERFACE => (),
        USB_RECIP_ENDPOINT => buf[0] = this.is_halted(ep_num, ep_dir) as u8,
        _ => {
            this.ep_halt(0, USB_RECV);
            return;
        }
    }
    let addr = buf.as_ptr() as usize;
    this.ep0_send(addr, 2, 0);
}

fn get_descriptor(this: &mut CorigineUsb, value: u16, index: u16, length: usize) {
    let Some(buf) = ep0_buf(this) else { return };
    let addr = buf.as_ptr() as usize;

    let len = match (value >> 8) as u8 {
        USB_DT_DEVICE => {
            let d = DeviceDescriptor::new();
            let n = length.min(size_of::<DeviceDescriptor>());
            buf[..n].copy_from_slice(&d.as_ref()[..n]);
            n
        }
        USB_DT_DEVICE_QUALIFIER => {
            // Describes what the device would look like at its *other* speed.
            // A high-speed capable device must answer this or some hosts will
            // refuse to switch.
            let q = QualifierDescriptor {
                b_length: size_of::<QualifierDescriptor>() as u8,
                b_descriptor_type: USB_DT_DEVICE_QUALIFIER,
                bcd_usb: 0x0200,
                b_device_class: 0,
                b_device_sub_class: 0,
                b_device_protocol: 0,
                b_max_packet_size0: 64,
                b_num_configurations: 1,
                b_reserved: 0,
            };
            let n = length.min(size_of::<QualifierDescriptor>());
            buf[..n].copy_from_slice(&q.as_ref()[..n]);
            n
        }
        USB_DT_CONFIG => {
            let (mps, interval) = match this.get_speed() {
                PortSpeed::Fs => (FS_BULK_MPS, hid::FS_INTR_INTERVAL),
                _ => (HS_BULK_MPS, hid::HS_INTR_INTERVAL),
            };
            write_config(buf, USB_DT_CONFIG, mps as u16, interval).min(length)
        }
        USB_DT_OTHER_SPEED_CONFIG => {
            // The mirror image: if we are running high speed, describe full
            // speed here, and vice versa.
            let (mps, interval) = match this.get_speed() {
                PortSpeed::Fs => (HS_BULK_MPS, hid::HS_INTR_INTERVAL),
                _ => (FS_BULK_MPS, hid::FS_INTR_INTERVAL),
            };
            write_config(buf, USB_DT_OTHER_SPEED_CONFIG, mps as u16, interval).min(length)
        }

        // The two HID descriptors are fetched with an ordinary GET_DESCRIPTOR
        // aimed at the interface rather than the device, so `wIndex` selects
        // which interface is being asked. Mass storage has neither.
        hid::USB_DT_HID if index == HID_INTERFACE as u16 => {
            let d = HidDescriptor::new(hid::REPORT_DESC.len() as u16);
            let n = length.min(size_of::<HidDescriptor>());
            buf[..n].copy_from_slice(&d.as_ref()[..n]);
            n
        }
        hid::USB_DT_HID if index == KBD_INTERFACE as u16 => {
            let d = HidDescriptor::new(kbd::REPORT_DESC.len() as u16);
            let n = length.min(size_of::<HidDescriptor>());
            buf[..n].copy_from_slice(&d.as_ref()[..n]);
            n
        }
        hid::USB_DT_HID_REPORT if index == HID_INTERFACE as u16 => {
            // The host asks for this once, at bind time, and a short or absent
            // answer is the difference between a working mouse and a device
            // that enumerates and then does nothing.
            let n = length.min(hid::REPORT_DESC.len()).min(buf.len());
            buf[..n].copy_from_slice(&hid::REPORT_DESC[..n]);
            n
        }
        hid::USB_DT_HID_REPORT if index == KBD_INTERFACE as u16 => {
            // Same story for the keyboard: without this the interface binds and
            // then no keystroke is ever understood.
            let n = length.min(kbd::REPORT_DESC.len()).min(buf.len());
            buf[..n].copy_from_slice(&kbd::REPORT_DESC[..n]);
            n
        }
        USB_DT_STRING => {
            let id = (value & 0xFF) as u8;
            if id == 0 {
                // Language list: US English only.
                buf[..4].copy_from_slice(&[4, USB_DT_STRING, 0x09, 0x04]);
                4.min(length)
            } else {
                let serial = super::serial_number();
                // Raw bytes rather than `&str`, because the product name is now
                // script-set and may not be ASCII. The writer widens each byte
                // to a UTF-16 unit either way; keeping bytes avoids a
                // `from_utf8` round trip and its failure case.
                let bytes: &[u8] = match id {
                    1 => MANUFACTURER.as_bytes(),
                    2 => product_name(),
                    3 => serial.as_bytes(),
                    _ => {
                        // Nothing else is advertised. Answering every index with
                        // the serial number, as boot1 does, means an OS probing
                        // for a Microsoft OS descriptor at index 0xEE gets a
                        // valid-looking string it will then try to interpret.
                        this.ep_halt(0, USB_RECV);
                        return;
                    }
                };
                // USB strings are UTF-16LE; ASCII bytes become one code unit
                // each. The length is a single byte, so clamp below 256 as well
                // as to the buffer -- a long string would otherwise write a
                // length that wrapped to something small.
                let slen = (2 + bytes.len() * 2).min(buf.len()).min(254);
                buf[0] = slen as u8;
                buf[1] = USB_DT_STRING;
                for (dst, &src) in buf[2..slen].chunks_exact_mut(2).zip(bytes) {
                    dst.copy_from_slice(&(src as u16).to_le_bytes());
                }
                slen.min(length)
            }
        }
        USB_DT_BOS => {
            let total = size_of::<BosDescriptor>() + size_of::<ExtCapDescriptor>();
            let bos = BosDescriptor {
                b_length: size_of::<BosDescriptor>() as u8,
                b_descriptor_type: USB_DT_BOS,
                w_total_length: total as u16,
                b_num_device_caps: 1,
            };
            let ext = ExtCapDescriptor {
                b_length: size_of::<ExtCapDescriptor>() as u8,
                b_descriptor_type: USB_DT_DEVICE_CAPABILITY,
                b_dev_capability_type: USB_CAP_TYPE_EXT,
                bm_attributes: (0xfa << 8) | (0x3 << 3),
            };
            let mut idx = 0;
            for p in [bos.as_ref(), ext.as_ref()] {
                buf[idx..idx + p.len()].copy_from_slice(p);
                idx += p.len();
            }
            total.min(length)
        }
        _ => {
            this.ep_halt(0, USB_RECV);
            return;
        }
    };
    this.ep0_send(addr, len, 0);
}

/// Class requests aimed at the mass-storage interface.
fn msc_class_request(this: &mut CorigineUsb, request: u8, w_value: u16, w_length: u16) {
    match request {
        BOT_REQ_RESET => {
            // The host's way of saying "we are out of sync, start over".
            // Re-arm the command receive; anything less leaves the drive
            // wedged until replug.
            if w_value == 0 && w_length == 0 {
                // No `ep_unhalt` here. Nothing halts a bulk endpoint in this
                // firmware, and the HAL's `ep_unhalt` busy-waits on the same
                // `EPRUNNING` field that makes `ep_halt` unsafe to call.
                // Re-arming is the whole of what recovery needs.
                msc::on_configured(this);
                this.ep0_send(0, 0, 0);
            } else {
                this.ep_halt(0, USB_RECV);
            }
        }
        BOT_REQ_GET_MAX_LUN => {
            // One logical unit, so the maximum index is 0. Hosts that get no
            // answer here enumerate the device and then never mount it.
            if w_value != 0 || w_length != 1 {
                this.ep_halt(0, USB_RECV);
            } else {
                let Some(buf) = ep0_buf(this) else { return };
                buf[0] = 0;
                let addr = buf.as_ptr() as usize;
                this.ep0_send(addr, 1, 0);
            }
        }
        _ => this.ep_halt(0, USB_RECV),
    }
}

/// Class requests aimed at the HID interface.
///
/// Only GET_REPORT is load-bearing -- a host may use it to read the pointer
/// state without waiting for an interrupt transfer. The rest exist because a
/// stalled SET_IDLE or SET_PROTOCOL during binding makes some drivers give up
/// on the interface entirely.
fn hid_class_request(this: &mut CorigineUsb, request: u8, w_value: u16, w_length: u16) {
    match request {
        hid::HID_REQ_GET_REPORT => {
            // wValue is (report type << 8) | report id. Only input reports
            // exist here, and there is a single unnumbered one.
            let report_type = (w_value >> 8) as u8;
            let report_id = (w_value & 0xff) as u8;
            if report_type != 1 || report_id != 0 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let report = hid::last_report();
            let n = (w_length as usize).min(hid::report_len());
            let Some(buf) = ep0_buf(this) else { return };
            buf[..n].copy_from_slice(&report[..n]);
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, n, 0);
        }
        hid::HID_REQ_GET_IDLE => {
            if w_length != 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let idle = hid::idle();
            let Some(buf) = ep0_buf(this) else { return };
            buf[0] = idle;
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, 1, 0);
        }
        hid::HID_REQ_SET_IDLE => {
            // High byte is the duration in 4 ms units, 0 meaning "report on
            // change only". Recorded so GET_IDLE can echo it; nothing here
            // repeats a report on its own, so it changes no behaviour.
            hid::set_idle((w_value >> 8) as u8);
            this.ep0_send(0, 0, 0);
        }
        hid::HID_REQ_GET_PROTOCOL => {
            if w_length != 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let p = hid::protocol();
            let Some(buf) = ep0_buf(this) else { return };
            buf[0] = p;
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, 1, 0);
        }
        hid::HID_REQ_SET_PROTOCOL => {
            // 0 = boot, 1 = report. The report descriptor's first three bytes
            // are the boot report, so both are served by the same buffer at
            // different lengths.
            if w_value > 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            hid::set_protocol(w_value as u8);
            this.ep0_send(0, 0, 0);
        }
        // A mouse has no output or feature reports, so there is nothing this
        // could set. Stalling is the defined answer for an unsupported report,
        // and unlike the requests above no host sends it during binding.
        hid::HID_REQ_SET_REPORT => this.ep_halt(0, USB_RECV),
        _ => this.ep_halt(0, USB_RECV),
    }
}

/// Class requests aimed at the keyboard interface.
///
/// Two of these are load-bearing in a way the mouse's are not. GET_REPORT lets a
/// host read the held keys without an interrupt transfer, as before -- but
/// SET_REPORT is how the host pushes the lock LEDs *down*, and that is the whole
/// substrate of the readback and the OS-detection trick. Because it carries a
/// data stage, it is handled by staging an EP0 receive and letting
/// [`handle_event`] copy the byte out when it lands.
fn hid_kbd_class_request(this: &mut CorigineUsb, request: u8, w_value: u16, w_length: u16) {
    match request {
        hid::HID_REQ_GET_REPORT => {
            let report_type = (w_value >> 8) as u8;
            let report_id = (w_value & 0xff) as u8;
            // Only the single unnumbered input report exists; an output report
            // read (type 2) or any id but zero is not something to invent.
            if report_type != 1 || report_id != 0 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let mut report = [0u8; kbd::REPORT_LEN];
            let report_len = kbd::current_report(&mut report);
            let n = (w_length as usize).min(report_len);
            let Some(buf) = ep0_buf(this) else { return };
            buf[..n].copy_from_slice(&report[..n]);
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, n, 0);
        }
        hid::HID_REQ_SET_REPORT => {
            // The host is pushing an output report down. wValue is
            // (report type << 8) | report id; type 2 is an output report, which
            // for a keyboard is the lock-LED byte. Anything else -- a feature
            // report, a numbered report -- this interface does not define.
            let report_type = (w_value >> 8) as u8;
            if report_type != 2 || w_length == 0 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            // Receive the LED byte into the EP0 staging buffer and mark that the
            // next EP0 OUT completion is that data, so `handle_event` copies it
            // into the readback state rather than ignoring it as it does every
            // other control status stage. Only the first byte is meaningful, but
            // the whole (short) report is accepted so the transfer completes.
            let p = this.ep0_buf.load(Ordering::SeqCst) as usize;
            let n = (w_length as usize).min(CRG_UDC_EP0_REQBUFSIZE);
            kbd::arm_led_capture();
            this.ep0_receive(p, n, 0);
        }
        hid::HID_REQ_GET_IDLE => {
            if w_length != 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let idle = kbd::idle();
            let Some(buf) = ep0_buf(this) else { return };
            buf[0] = idle;
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, 1, 0);
        }
        hid::HID_REQ_SET_IDLE => {
            kbd::set_idle((w_value >> 8) as u8);
            this.ep0_send(0, 0, 0);
        }
        hid::HID_REQ_GET_PROTOCOL => {
            if w_length != 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            let p = kbd::protocol();
            let Some(buf) = ep0_buf(this) else { return };
            buf[0] = p;
            let addr = buf.as_ptr() as usize;
            this.ep0_send(addr, 1, 0);
        }
        hid::HID_REQ_SET_PROTOCOL => {
            if w_value > 1 {
                this.ep_halt(0, USB_RECV);
                return;
            }
            kbd::set_protocol(w_value as u8);
            this.ep0_send(0, 0, 0);
        }
        _ => this.ep_halt(0, USB_RECV),
    }
}

/// The driver's event callback. Everything USB that is not a bulk data
/// completion arrives here.
pub fn handle_event(this: &mut CorigineUsb, event_trb: &mut EventTrbS) -> CrgEvent {
    let pei = event_trb.get_endpoint_id();
    let mut ret = CrgEvent::None;

    match event_trb.get_trb_type() {
        TrbType::EventPortStatusChange => {
            let portsc_val = this.csr.r(bao1x_hal::usb::utra::PORTSC);
            this.csr.wo(bao1x_hal::usb::utra::PORTSC, portsc_val);
            let portsc = PortSc(portsc_val);
            if portsc.prc() && !portsc.pr() {
                this.update_current_speed();
            }
            if portsc.csc() && portsc.ppc() && portsc.pp() && portsc.ccs() {
                this.update_current_speed();
            }
            this.csr.rmwf(bao1x_hal::usb::utra::EVENTCONFIG_SETUP_ENABLE, 1);
        }

        TrbType::EventTransfer => {
            // The endpoint index is reported by hardware and is about to be
            // used to index a fixed array. boot1 indexes it unchecked, which
            // turns a controller glitch into a panic; bail out instead.
            if pei as usize >= this.udc_ep.len() {
                return CrgEvent::None;
            }
            let Ok(comp_code) = CompletionCode::try_from(event_trb.dw2.compl_code()) else {
                return CrgEvent::None;
            };
            let residual_length = event_trb.dw2.trb_tran_len() as u16;

            // Advance the dequeue pointer past the TRB the hardware just
            // finished. The pointer came from hardware, so it is bounds-checked
            // before being dereferenced.
            if !super::ptr_in_udc_window(event_trb.dw0 as usize, size_of::<TransferTrbS>() * 2) {
                return CrgEvent::None;
            }
            // safety: the range check above covers both this TRB and the next.
            let deq_pt = unsafe { &mut *(event_trb.dw0 as *mut TransferTrbS).add(1) };
            let udc_ep = &mut this.udc_ep[pei as usize];
            if deq_pt.get_trb_type() == TrbType::Link {
                udc_ep.deq_pt = core::sync::atomic::AtomicPtr::new(udc_ep.first_trb.load(Ordering::SeqCst));
            } else {
                udc_ep.deq_pt = core::sync::atomic::AtomicPtr::new(deq_pt as *mut TransferTrbS);
            }

            let dir = (pei & 1) != 0;
            if pei == 0 {
                if comp_code == CompletionCode::Success {
                    // The direction bits look inverted here. They are copied
                    // from boot1, which notes the same thing: this is what
                    // actually causes the next control packet to move.
                    ret = if dir == USB_SEND { CrgEvent::Data(0, 1, 0) } else { CrgEvent::Data(1, 0, 0) };
                }
            } else if pei == 1 {
                // EP0 OUT completions. The only one this firmware acts on is the
                // data stage of a keyboard lock-LED SET_REPORT, which
                // `hid_kbd_class_request` armed just before staging the receive.
                // Every other EP0 OUT event -- the status stage of an IN control
                // transfer, a SET_SEL payload -- means nothing here, so the
                // armed flag is what separates them. Control transfers are
                // serialized on EP0, so the first OUT completion after arming is
                // that data and no other can slip in front of it.
                if matches!(comp_code, CompletionCode::Success | CompletionCode::ShortPacket)
                    && kbd::take_led_capture()
                {
                    if let Some(buf) = ep0_buf(this) {
                        kbd::on_host_leds(buf[0]);
                    }
                }
            } else if pei >= 2 && matches!(comp_code, CompletionCode::Success | CompletionCode::ShortPacket) {
                if let Some(f) = this.udc_ep[pei as usize].completion_handler {
                    if super::ptr_in_udc_window(event_trb.dw0 as usize, size_of::<TransferTrbS>()) {
                        // safety: bounds-checked above; the driver only ever
                        // puts TRBs it allocated into the event ring.
                        let p_trb = unsafe { &*(event_trb.dw0 as *const TransferTrbS) };
                        f(this, p_trb.dplo as usize, p_trb.dw2.0, 0, residual_length);
                    }
                }
            }
        }

        TrbType::SetupPkt => {
            let raw = event_trb.get_raw_setup();
            this.setup = Some(raw);
            this.setup_tag = event_trb.get_setup_tag();
            let req = CtrlRequest::from_bytes(&raw);
            // Copy out of the packed struct before use; a reference to a packed
            // field is not allowed.
            let (w_value, w_index, w_length) = (req.w_value, req.w_index, req.w_length);

            match req.b_request_type & USB_TYPE_MASK {
                USB_TYPE_STANDARD => match req.b_request {
                    USB_REQ_GET_STATUS => get_status(this, req.b_request_type, w_index),
                    USB_REQ_SET_ADDRESS => this.set_addr(w_value as u8, CRG_INT_TARGET),
                    USB_REQ_SET_ISOCH_DELAY => this.ep0_send(0, 0, 0),
                    USB_REQ_SET_SEL => {
                        // Six bytes by spec. `wLength` is host-supplied and
                        // lands as the length of a DMA into a 256-byte buffer,
                        // so it is clamped rather than trusted -- boot1 passes
                        // it through unchecked.
                        let p = this.ep0_buf.load(Ordering::SeqCst) as usize;
                        let n = (w_length as usize).min(CRG_UDC_EP0_REQBUFSIZE);
                        this.ep0_receive(p, n, 0);
                    }
                    USB_REQ_CLEAR_FEATURE => {
                        // CLEAR_FEATURE(ENDPOINT_HALT) is how a host recovers a
                        // stalled bulk endpoint, and it is a required step of
                        // the mass-storage reset sequence. Stalling it, as
                        // boot1 does, means the host can never finish that
                        // recovery.
                        //
                        // There is nothing to actually clear: this firmware
                        // never halts a bulk endpoint (see `msc.rs` on why the
                        // HAL's `ep_halt` is avoided), so acknowledging is both
                        // honest and sufficient.
                        if (req.b_request_type & 0x1f) == USB_RECIP_ENDPOINT && w_value == 0 {
                            this.ep0_send(0, 0, 0);
                        } else {
                            this.ep_halt(0, USB_RECV);
                        }
                    }
                    USB_REQ_SET_FEATURE => this.ep_halt(0, USB_RECV),
                    USB_REQ_SET_CONFIGURATION => match w_value {
                        0 => {
                            // Deconfigured: the drive is no longer mounted, and
                            // the status screen should stop claiming it is.
                            this.set_device_state(UsbDeviceState::Address);
                            msc::on_deconfigured();
                            hid::on_deconfigured();
                            kbd::on_deconfigured();
                            this.ep0_send(0, 0, 0);
                        }
                        1 => {
                            this.set_device_state(UsbDeviceState::Configured);
                            msc::on_configured(this);
                            hid::on_configured(this);
                            kbd::on_configured(this);
                            this.ep0_send(0, 0, 0);
                        }
                        _ => this.ep_halt(0, USB_RECV),
                    },
                    USB_REQ_GET_DESCRIPTOR => get_descriptor(this, w_value, w_index, w_length as usize),
                    USB_REQ_GET_CONFIGURATION => {
                        let Some(buf) = ep0_buf(this) else { return CrgEvent::None };
                        buf[0] = (this.get_device_state() == UsbDeviceState::Configured) as u8;
                        let addr = buf.as_ptr() as usize;
                        this.ep0_send(addr, 1, 0);
                    }
                    // `wIndex` is the interface and `wValue` is the alternate
                    // setting. Both interfaces have exactly one, so the only
                    // legal request is "set interface 0 or 1 to alt 0", and the
                    // only legal answer to GET_INTERFACE is zero.
                    //
                    // boot1 stores `wValue` and hands it back regardless of
                    // which interface was asked about, which with one interface
                    // is invisible and with two is simply wrong.
                    USB_REQ_SET_INTERFACE => {
                        if w_index < NUM_INTERFACES as u16 && w_value == 0 {
                            this.cur_interface_num = w_index as u8;
                            this.ep0_send(0, 0, 0);
                        } else {
                            this.ep_halt(0, USB_RECV);
                        }
                    }
                    USB_REQ_GET_INTERFACE => {
                        if w_index >= NUM_INTERFACES as u16 {
                            this.ep_halt(0, USB_RECV);
                            return CrgEvent::Data(0, 0, 1);
                        }
                        let Some(buf) = ep0_buf(this) else { return CrgEvent::None };
                        buf[0] = 0;
                        let addr = buf.as_ptr() as usize;
                        this.ep0_send(addr, 1, 0);
                    }
                    _ => this.ep_halt(0, USB_RECV),
                },

                // Both functions' class requests are addressed to an interface,
                // and `wIndex` is the only thing that tells them apart. boot1
                // dispatches on `bRequest` alone, which is exactly why its
                // Bulk-Only Reset checks the wrong interface number in a
                // composite layout.
                USB_TYPE_CLASS => {
                    if (req.b_request_type & 0x1f) != USB_RECIP_INTERFACE {
                        this.ep_halt(0, USB_RECV);
                    } else {
                        match (w_index & 0xff) as u8 {
                            MSC_INTERFACE => msc_class_request(this, req.b_request, w_value, w_length),
                            HID_INTERFACE => hid_class_request(this, req.b_request, w_value, w_length),
                            KBD_INTERFACE => hid_kbd_class_request(this, req.b_request, w_value, w_length),
                            _ => this.ep_halt(0, USB_RECV),
                        }
                    }
                }

                _ => this.ep_halt(0, USB_RECV),
            }
            ret = CrgEvent::Data(0, 0, 1);
        }

        _ => (),
    }
    ret
}
