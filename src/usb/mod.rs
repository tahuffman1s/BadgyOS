//! USB mass storage and a HID mouse, serviced from the main polling loop.
//!
//! # Why polling
//!
//! boot1 and xous-core's `baremetal` both drive this controller from an
//! interrupt, and boot1's own comment says "USB is entirely interrupt driven,
//! so there is no loop to handle it". That is a description of how they are
//! built, not a requirement of the hardware. Their interrupt handler
//! (`baremetal/src/platform/bao1x/irq.rs:202-250`) is a hand-inlined copy of
//! [`CorigineUsb::udc_handle_interrupt`], which reads `USBSTS`, drains the
//! event ring, and re-arms. Nothing in it depends on having arrived via a trap:
//! the event ring is written by hardware and read by walking a cycle bit.
//!
//! So this module calls that same function from the poll loop, and the firmware
//! keeps its "no interrupts, no threads, one loop" shape. That is worth real
//! money here -- taking an interrupt would mean porting a 258-line trap
//! trampoline, and would put the allocator's spin lock and the ReRAM commit
//! sequence (which the HAL documents as unsafe under concurrency) in reach of
//! reentrancy.
//!
//! The cost is latency. The longest the loop can go without polling is one
//! `Oled128x128::draw()`, about 14 ms, which is inside the 50 ms a host allows
//! for a `SET_ADDRESS` and far inside anything bulk. Bulk transfers that are
//! not serviced immediately are simply NAKed and retried, which is normal USB.

pub mod hid;
pub mod msc;
mod proto;

use alloc::string::String;

use bao1x_api::{IoGpio, IoxPort, IoxValue};
use bao1x_hal::iox::Iox;
use bao1x_hal::usb::compat::AtomicCsr;
use bao1x_hal::usb::driver::{CorigineUsb, PortSpeed, UsbDeviceState};

use crate::platform::delay;

/// The controller. `None` until [`attach`] runs, which keeps it in `.bss`.
static mut USB: Option<CorigineUsb> = None;

/// Bounds check for any pointer the hardware hands back.
///
/// Every address in an event TRB is produced by the UDC, and this firmware
/// dereferences several of them. They should always point inside the two
/// windows the driver allocated; if one ever does not, that is either a
/// hardware fault or an attack, and either way dereferencing it would be an
/// arbitrary write.
pub fn ptr_in_udc_window(base: usize, len: usize) -> bool {
    let Some(end) = base.checked_add(len) else {
        return false;
    };
    let udc = bao1x_hal::board::CRG_UDC_MEMBASE;
    let udc_end = udc + bao1x_hal::board::CRG_IFRAM_PAGES * 4096;
    let app = utralib::HW_IFRAM1_MEM;
    let app_end = app + 4096 * 2;
    (base >= udc && end <= udc_end) || (base >= app && end <= app_end)
}

/// A per-device serial number, so two badges plugged into the same machine are
/// distinguishable.
pub fn serial_number() -> String {
    let owc = bao1x_hal::acram::OneWayCounter::new();
    let slots = bao1x_hal::acram::SlotManager::new();
    bao1x_hal::usb::derive_usb_serial_number(&owc, &slots)
}

/// A 32-bit value derived from the device serial, for the FAT volume ID.
///
/// Windows uses the volume ID together with the label to tell two removable
/// volumes apart; two badges with the same one will not both get a drive letter.
pub fn volume_id() -> u32 {
    let sn = serial_number();
    let mut h: u32 = 0x811c_9dc5;
    for b in sn.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Bring up the device controller and let the host see it.
///
/// The port arrives held in SE0 -- boot1 drives PF5 low in its own early init
/// and, on the update-mode path, explicitly stops the controller and re-asserts
/// SE0 before jumping here so that the next stage can enumerate cleanly. So
/// there is nothing to adopt and no state to preserve: assert SE0, build the
/// controller, then release SE0 and let the host discover a new device.
pub fn attach(iox: &Iox) {
    let (se0_port, se0_pin) = bao1x_hal::board::setup_usb_pins(iox);
    iox.set_gpio_pin_value(se0_port, se0_pin, IoxValue::Low);
    delay(100);

    // safety: called once, before anything else touches `USB`.
    let usb = unsafe {
        CorigineUsb::new(
            bao1x_hal::board::CRG_UDC_MEMBASE,
            AtomicCsr::new(bao1x_hal::usb::utra::CORIGINE_USB_BASE as *mut u32),
            AtomicCsr::new(utralib::utra::irqarray1::HW_IRQARRAY1_BASE as *mut u32),
        )
    };
    // safety: single-threaded, and this is the only writer of `USB`.
    unsafe {
        USB = Some(usb);
        let Some(u) = (*core::ptr::addr_of_mut!(USB)).as_mut() else { return };
        u.assign_handler(proto::handle_event);
        u.reset();
        // `None` advertises both full and high speed and lets the host pick.
        u.init(None);
        u.start();
        u.update_current_speed();
        // Interrupts are never unmasked at the CPU, so this only marks the
        // controller's own enable bits; the poll loop does the servicing.
        u.irq_csr.wo(utralib::utra::irqarray1::EV_PENDING, 0xffff_ffff);
    }

    delay(100);
    iox.set_gpio_pin_value(se0_port, se0_pin, IoxValue::High);
    crate::println!("USB: attached as {:04x}:{:04x}", proto::vendor_id(), proto::product_id());
}

/// Service the controller. Cheap when there is nothing to do -- one register
/// read -- so the loop can call it as often as it likes.
pub fn poll() {
    // safety: single-threaded, no interrupts, so no other borrow can exist.
    unsafe {
        if let Some(u) = (*core::ptr::addr_of_mut!(USB)).as_mut() {
            u.udc_handle_interrupt();
        }
    }
}

/// Reach the controller from code that is not already holding it -- the HID
/// module, which queues a report when a script asks rather than in response to
/// an event. Returns `None` before [`attach`] and between [`reattach`]'s two
/// halves.
///
/// Must not be called from inside an event or completion handler: those already
/// hold a `&mut` to the same controller, and this would alias it. Nothing does,
/// and nothing can accidentally start to -- there are no interrupts and no
/// threads, so the only way in is a direct call.
pub fn with_usb<R>(f: impl FnOnce(&mut CorigineUsb) -> R) -> Option<R> {
    // safety: as `poll`. Single-threaded with no interrupts, so the borrow this
    // hands out cannot overlap another.
    unsafe { (*core::ptr::addr_of_mut!(USB)).as_mut().map(f) }
}

/// Drop off the bus and come back, so the host throws away everything it
/// believed about the volume.
///
/// This is the blunt instrument, and it is used after a reformat. The polite
/// alternative -- reporting a unit attention with "medium may have changed" --
/// is not reliably honoured: Linux, macOS and Windows all treat a cached FAT
/// differently, and a host that keeps its stale directory will write it back
/// over the new one and corrupt the volume for real. A disconnect leaves no
/// room for interpretation.
///
/// The controller is stopped before being rebuilt because `init()` hangs on a
/// controller that is still running; boot1 hit the same thing and says so at
/// its own shutdown call site.
pub fn reattach() {
    // safety: as `poll`; nothing else holds a reference across this.
    unsafe {
        if let Some(u) = (*core::ptr::addr_of_mut!(USB)).as_mut() {
            u.stop();
        }
        USB = None;
    }
    msc::CONFIGURED.store(false, core::sync::atomic::Ordering::SeqCst);
    hid::on_deconfigured();

    let iox = Iox::new(utralib::utra::iox::HW_IOX_BASE as *mut u32);
    iox.set_gpio_pin_value(IoxPort::PF, 5, IoxValue::Low);
    // Long enough that a host registers the disconnect rather than a glitch.
    delay(300);
    attach(&iox);
}

/// Has the host configured the interface? The closest thing to "the drive is
/// mounted" that the device side can observe.
pub fn is_configured() -> bool { msc::CONFIGURED.load(core::sync::atomic::Ordering::SeqCst) }

/// The vendor and product id the device is currently presenting.
pub fn ids() -> (u16, u16) { (proto::vendor_id(), proto::product_id()) }

/// Change the vendor/product id the device presents, and re-enumerate so a host
/// sees the new identity.
///
/// This is a subsystem entry point in the same shape as everything else a
/// script can reach: it does one concrete thing to the hardware and reports
/// whether it took. It returns false without touching the bus for an identity
/// [`proto::set_identity`] refuses (the bootloader's reserved id); otherwise it
/// drops off the bus and comes back under the new id and returns true.
pub fn set_identity(vid: u16, pid: u16) -> bool {
    if !proto::set_identity(vid, pid) {
        return false;
    }
    reattach();
    true
}

/// Change the product string the host shows as the device name, and
/// re-enumerate so the change is visible. An empty name restores the default.
pub fn set_product_name(name: &str) {
    proto::set_product_name(name);
    reattach();
}

/// Link state and negotiated speed, as short strings for the status screen.
///
/// Neither enum implements `Debug` in the HAL, and the names it does use
/// (`SspGen2x2` and friends) are wider than the panel anyway.
pub fn status() -> (&'static str, &'static str) {
    // safety: as `poll`.
    let (state, speed) = unsafe {
        match (*core::ptr::addr_of_mut!(USB)).as_mut() {
            Some(u) => (u.get_device_state(), u.get_speed()),
            None => (UsbDeviceState::NotAttached, PortSpeed::Invalid),
        }
    };
    let state = match state {
        UsbDeviceState::NotAttached => "off",
        UsbDeviceState::Attached => "attached",
        UsbDeviceState::Powered => "powered",
        UsbDeviceState::Reconnecting => "reconnect",
        UsbDeviceState::Unauthenticated => "unauth",
        UsbDeviceState::Default => "default",
        UsbDeviceState::Address => "addressed",
        UsbDeviceState::Configured => "configured",
        UsbDeviceState::Suspended => "suspended",
    };
    let speed = match speed {
        PortSpeed::Fs => "full",
        PortSpeed::Ls => "low",
        PortSpeed::Hs => "high",
        PortSpeed::Ss => "super",
        PortSpeed::SspGen2x1 | PortSpeed::SspGen1x2 | PortSpeed::SspGen2x2 => "super+",
        PortSpeed::Invalid => "-",
    };
    (state, speed)
}
