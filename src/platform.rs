//! Minimum viable bring-up for the bao1x on a baosec-class board.
//!
//! Vendored down from `xous-core/baremetal/src/platform/bao1x/bao1x.rs`, keeping
//! only what is needed to have a working clock, timer, heap, console and OLED:
//! no interrupts, no USB, no BIO, no camera/keyboard/flash, no TRNG.

use bao1x_api::*;
use bao1x_hal::iox::Iox;
use utralib::CSR;
use utralib::utra;

#[global_allocator]
static ALLOCATOR: linked_list_allocator::LockedHeap = linked_list_allocator::LockedHeap::empty();

pub const RAM_SIZE: usize = utralib::generated::HW_SRAM_MEM_LEN;
pub const RAM_BASE: usize = utralib::generated::HW_SRAM_MEM;

/// Must match the `--sig-length` handed to `xous-sign-image`, and the FLASH ORIGIN
/// baked into `link.x`.
pub const SIGBLOCK_LEN: usize = 768;

/// Size of the heap. Its *start* is not a constant -- see `setup_alloc`.
///
/// Sized from measurement, not from taste. Parsing costs roughly 25 KB per KiB
/// of source -- the token vector and the AST arena are both live at once -- so
/// the 16 KiB script ceiling in `scripts.rs` peaks around 250 KB, and the
/// script's own values need room after that. `linked_list_allocator` is
/// first-fit and will fragment across successive runs, which argues for slack
/// rather than a tight fit.
///
/// The ceiling is where the heap would meet the descending stack. With .bss at
/// 536 KiB the gap is about 1.5 MB, so 768 KiB still leaves ~740 KB of stack
/// against a peak measured in tens of KB.
///
/// The RAM disk is *not* in here -- it is a `.bss` array, so it costs no heap
/// and no image bytes.
pub const HEAP_LEN: usize = 1024 * 768;

pub const UART_IFRAM_ADDR: usize = bao1x_hal::board::UART_DMA_TX_BUF_PHYS;

pub const SYSTEM_CLOCK_FREQUENCY: u32 = 700_000_000;

/// Bring the chip up far enough to talk to the console and the display.
/// Returns `perclk`, which the SPIM (and therefore the OLED) needs.
///
/// Assumes boot1 ran first and left the board's keep-on rails and basic pins alone.
pub fn early_init() -> u32 {
    // Ensure SRAM timings are set for 900mV operation before raising the clock
    // frequency -- we run at full tilt on baosec.
    let trim_table =
        bao1x_hal::sram_trim::get_sram_trim_for_voltage(bao1x_api::offsets::dabao::CPU_VDD_LDO_BOOT_MV);
    let mut rbist = CSR::new(utra::rbist_wrp::HW_RBIST_WRP_BASE as *mut u32);
    for item in trim_table {
        rbist.wo(utra::rbist_wrp::SFRCR_TRM, item.raw_value());
        rbist.wo(utra::rbist_wrp::SFRAR_TRM, 0x5a);
    }

    // Now that the SRAM trims are set up, initialize all the statics by writing to
    // memory. For a baremetal image the statics structure sits right after the
    // signature block at the start of flash; `xous-copy-object --bao1x` produced it.
    const STATICS_LOC: usize = bao1x_api::BAREMETAL_START + SIGBLOCK_LEN;

    // safety: this data structure is pre-loaded by the image builder and is
    // guaranteed to only have representable, valid values aligned per repr(C).
    let statics_in_rom: &bao1x_api::StaticsInRom =
        unsafe { (STATICS_LOC as *const bao1x_api::StaticsInRom).as_ref().unwrap() };
    assert!(statics_in_rom.version == bao1x_api::STATICS_IN_ROM_VERSION, "Can't find valid statics table");

    // Clear the .data/.bss region, then apply the poke table to set .data values.
    // safety: only safe if the values computed by the image builder are correct.
    unsafe {
        let data_ptr = statics_in_rom.data_origin as *mut u32;
        for i in 0..statics_in_rom.data_size_bytes as usize / size_of::<u32>() {
            data_ptr.add(i).write_volatile(0);
        }
        for &(offset, data) in &statics_in_rom.poke_table[..statics_in_rom.valid_pokes as usize] {
            data_ptr
                .add(u16::from_le_bytes(offset) as usize / size_of::<u32>())
                .write_volatile(u32::from_le_bytes(data));
        }
    }

    // Set the clock. This also gives us perclk, which the UART and SPIM divide down.
    let perclk = unsafe {
        bao1x_hal::clocks::init_clock_asic(
            SYSTEM_CLOCK_FREQUENCY,
            utra::sysctrl::HW_SYSCTRL_BASE,
            utralib::HW_AO_SYSCTRL_BASE,
            Some(utra::duart::HW_DUART_BASE),
            delay_at_sysfreq,
            true,
        )
    };

    // Place the heap immediately above the region we just cleared.
    setup_alloc(statics_in_rom.data_origin as usize + statics_in_rom.data_size_bytes as usize);
    setup_timer();

    // Console up. Everything before this point can only print on the DUART.
    let mut udma_uart = crate::debug::setup_tx(perclk);
    udma_uart.write("BadgyOS console up\r\n".as_bytes());
    crate::debug::USE_CONSOLE.store(true, core::sync::atomic::Ordering::SeqCst);

    setup_display_power();

    perclk
}

/// Reset the panel and switch on its power rail.
///
/// `Oled128x128::new()` configures the display pins, the reset line and the power
/// pin, but it never *drives* the power pin -- on a stock badge that write happens
/// once, in boot1. The Xous loader just inherits that state. We do it explicitly
/// instead, so this firmware still comes up if it is entered from a boot1 that
/// took a different path. The sequence mirrors boot1's:
/// reset asserted -> pins configured -> reset released -> power on.
fn setup_display_power() {
    let iox = Iox::new(utra::iox::HW_IOX_BASE as *mut u32);

    bao1x_hal::board::setup_periph_reset_pin(&iox);
    bao1x_hal::board::assert_periph_reset(&iox, true);
    // Configure the display pins while reset is held, which doubles as the delay.
    bao1x_hal::board::setup_display_pins(&iox);
    let (oled_on_port, oled_on_pin) = bao1x_hal::board::setup_oled_power_pin(&iox);
    bao1x_hal::board::assert_periph_reset(&iox, false);

    iox.set_gpio_pin_value(oled_on_port, oled_on_pin, IoxValue::High);
}

pub fn setup_timer() {
    // timer0 is polled by delay(); no interrupts are used.
    let mut timer = CSR::new(utra::timer0::HW_TIMER0_BASE as *mut u32);
    timer.wfo(utra::timer0::EV_ENABLE_ZERO, 0);
    timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);

    let ms = 1;
    timer.wfo(utra::timer0::EN_EN, 0b0); // disable the timer
    timer.wfo(utra::timer0::LOAD_LOAD, 0);
    timer.wfo(utra::timer0::RELOAD_RELOAD, (SYSTEM_CLOCK_FREQUENCY / 1_000) * ms);
    timer.wfo(utra::timer0::EN_EN, 0b1);
}

/// Initialize the heap directly above the cleared .data/.bss/.stack region.
///
/// `heap_start` is derived from the statics table rather than hardcoded on
/// purpose. link.x sizes the (fictitious) `.stack` section with
/// `. += 16K; . = ALIGN(4096)`, so the linker's `_sheap` hops to the next 4 KiB
/// boundary as soon as .data crosses one. xous-core's `baremetal` pins its
/// `HEAP_START` to `RAM_BASE + 0x6000` and happens to agree today; grow .data a
/// couple of KiB there and the zeroize loop in `early_init` starts clearing the
/// first page of the heap. Deriving it removes the cliff.
///
/// Note the real stack is NOT the linker's `.stack`: `_start` sets sp to the top
/// of RAM, so the heap has all of SRAM above it minus the descending stack.
pub fn setup_alloc(heap_start: usize) {
    assert!(heap_start >= RAM_BASE, "heap starts below SRAM");
    assert!(heap_start + HEAP_LEN < RAM_BASE + RAM_SIZE, "heap does not fit in SRAM");
    // safety: the range is inside SRAM, sits above everything the statics table
    // claims, and below the stack descending from the top of RAM.
    unsafe {
        ALLOCATOR.lock().init(heap_start as *mut u8, HEAP_LEN);
    }
}

/// Delay at an explicit system clock frequency. Used during clock bring-up, when
/// timer0's reload value has not been programmed yet.
pub fn delay_at_sysfreq(ms: usize, sysclk_freq: u32) {
    let mut timer = CSR::new(utra::timer0::HW_TIMER0_BASE as *mut u32);
    timer.wfo(utra::timer0::EN_EN, 0b0);
    timer.wfo(utra::timer0::LOAD_LOAD, 0);
    timer.wfo(utra::timer0::RELOAD_RELOAD, sysclk_freq / 1000);
    timer.wfo(utra::timer0::EN_EN, 1);
    timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    for _ in 0..ms {
        while timer.rf(utra::timer0::EV_PENDING_ZERO) == 0 {}
        timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    }
}

/// Bytes of heap still free.
///
/// `linked_list_allocator` tracks this itself; it is a walk of the free list,
/// so it is cheap but not free -- call it on a tick, not in an inner loop.
pub fn heap_free() -> usize { ALLOCATOR.lock().free() }

/// Heap that must stay free for the firmware to keep working after a script has
/// finished with it: enough to format an error, rebuild the menu, and re-read a
/// script off the drive.
pub const HEAP_RESERVE: usize = 192 * 1024;

/// Delay a given number of milliseconds. Requires `setup_timer()` first.
///
/// Nothing else runs while this spins. Once USB is up, prefer
/// [`delay_polled`] -- a device that stops answering for a whole delay is a
/// device the host starts to doubt.
pub fn delay(ms: usize) {
    let mut timer = CSR::new(utra::timer0::HW_TIMER0_BASE as *mut u32);
    timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    for _ in 0..ms {
        while timer.rf(utra::timer0::EV_PENDING_ZERO) == 0 {}
        timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    }
}

/// Like [`delay`], but runs `f` while it waits.
///
/// This is how the USB controller gets serviced during every pause in the
/// firmware. The callback runs in the inner busy-wait rather than once per
/// millisecond, so the controller is polled thousands of times per millisecond
/// -- which costs one register read each and keeps the worst-case service gap
/// down to whatever the *caller* does between delays.
pub fn delay_polled(ms: usize, f: &mut dyn FnMut()) {
    let mut timer = CSR::new(utra::timer0::HW_TIMER0_BASE as *mut u32);
    timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    for _ in 0..ms {
        while timer.rf(utra::timer0::EV_PENDING_ZERO) == 0 {
            f();
        }
        timer.wfo(utra::timer0::EV_PENDING_ZERO, 1);
    }
}

mod panic_handler {
    use core::panic::PanicInfo;

    #[panic_handler]
    fn handle_panic(arg: &PanicInfo) -> ! {
        crate::println!("{}", arg);
        loop {}
    }
}
