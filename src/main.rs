//! BadgyOS -- a small, scriptable DEF CON 34 badge firmware, with a badger.
//!
//! Everything the stock image does is gone: no Xous kernel, no processes, no
//! swap, no PDDB, no FIDO2/TOTP/vault, no LED genetics. This is a no_std program
//! with no interrupts that boot1 jumps straight into.
//!
//! It brings up the clock tree, the console UART, the SH1107 OLED, the key
//! matrix and a USB mass-storage device, then runs a jog-wheel-driven UI: a home
//! screen with Badgy on it, a menu of demos and diagnostics, and -- the point of
//! the exercise -- a list of `pycon` scripts that were dragged onto the badge's
//! USB drive.
//!
//! Those scripts run as cooperative tasks, up to three at once, each on its own
//! stack and drawing into its own page: see [`sched`]. That is the one place
//! this firmware is not the single polling loop it started as -- and it is still
//! one loop, handed around.
//!
//! ```text
//!   host  --USB MSC-->  RAM disk  --FAT12-->  scripts  --interpreter-->  panel
//!                          |
//!                       ReRAM (so they survive a power cycle)
//! ```
//!
//! Flashes to the loader slot (`BAREMETAL_START == LOADER_START == 0x6006_0000`),
//! which means it *replaces* the OS -- see README.md before putting it on a badge.

#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod anim;
mod app;
mod asm;
mod badgy;
mod debug;
mod gfx;
mod input;
mod menu;
mod platform;
mod runner;
mod sched;
mod scripts;
mod sprites;
mod store;
mod usb;
mod util;

use bao1x_hal::iox::Iox;
use bao1x_hal::sh1107::Oled128x128;
use bao1x_hal::udma::GlobalConfig;
use utralib::utra;

use crate::platform::delay;

pub const UART_BAUD: u32 = bao1x_api::UART_BAUD;

/// Entrypoint, reached from `_start` in asm.rs.
///
/// # Safety
///
/// This function is safe to call exactly once.
#[export_name = "rust_entry"]
pub unsafe extern "C" fn rust_entry() -> ! {
    let perclk = platform::early_init();

    println!();
    println!("=== BadgyOS ===");
    println!("CPU {} MHz / perclk {} MHz", platform::SYSTEM_CLOCK_FREQUENCY / 2, perclk / 1_000_000);

    // ---- display ----
    // `Oled128x128::new` claims the display pins, the OLED power rail and the
    // peripheral reset line on its own, so there is nothing to set up first.
    let iox = Iox::new(utra::iox::HW_IOX_BASE as *mut u32);
    let udma_global = GlobalConfig::new();
    let mut sh1107 = Oled128x128::new(bao1x_hal::sh1107::MainThreadToken::new(), perclk, &iox, &udma_global);

    sh1107.init().expect("couldn't initialize the OLED");
    // The panel needs ~100ms after reset before it reliably takes pixel data.
    delay(100);

    // ---- input ----
    // The wheel and the three face buttons hang off port PF; see input.rs for
    // the matrix. This claims those pins, so it has to come after the display
    // brings up its own.
    let mut keys = input::Keys::new();

    // ---- script volume ----
    // Before USB, deliberately. On a first boot this formats the volume and
    // writes half a megabyte to ReRAM, which takes long enough that a host
    // watching the drive would see it stop answering. With the port still held
    // in SE0 there is no host to notice.
    let mut app = app::App::new(perclk);
    app.init_storage();

    // ---- USB ----
    // The port arrives held in SE0 -- boot1 puts it there so the next stage can
    // enumerate cleanly -- so this is a fresh attach rather than a handover.
    usb::attach(&iox);

    println!("wheel + buttons ready; entering UI.");

    app.run(&mut sh1107, &mut keys)
}
