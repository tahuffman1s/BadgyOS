//! Transmit-only serial console.
//!
//! Two sinks, selected by [`USE_CONSOLE`]:
//!   - before the UDMA UART is clocked and configured, bytes go out the always-on DUART (useful for very
//!     early bring-up, needs a probe on the debug pads);
//!   - afterwards, out UART2 on the board's console pins (PB14 = TX, PB13 = RX), at `bao1x_api::UART_BAUD` (1
//!     Mbaud), 8N1.
//!
//! There is no receive path and no interrupt handling: this firmware never reads
//! from the console.

use core::fmt::{Error, Write};
use core::sync::atomic::{AtomicBool, Ordering::SeqCst};

use bao1x_api::*;
use bao1x_hal::iox::Iox;
use bao1x_hal::{udma, udma::GlobalConfig};
use utralib::generated::*;

pub static USE_CONSOLE: AtomicBool = AtomicBool::new(false);

pub struct Uart {}

impl Uart {
    pub fn putc(&self, c: u8) {
        if !USE_CONSOLE.load(SeqCst) {
            let mut uart = CSR::new(utra::duart::HW_DUART_BASE as *mut u32);
            if uart.rf(utra::duart::SFR_CR_SFR_CR) == 0 {
                uart.wfo(utra::duart::SFR_CR_SFR_CR, 1);
            }
            while uart.r(utra::duart::SFR_SR) != 0 {}
            uart.wo(utra::duart::SFR_TXD, c as u32);
        } else {
            let buf: [u8; 1] = [c];
            // safety: safe to call, because setup_tx() turned on the clock and
            // configured the peripheral before USE_CONSOLE was set.
            let mut udma_uart = unsafe {
                udma::Uart::get_handle(
                    utra::udma_uart_2::HW_UDMA_UART_2_BASE,
                    crate::platform::UART_IFRAM_ADDR,
                    crate::platform::UART_IFRAM_ADDR,
                )
            };
            udma_uart.write(&buf);
        }
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> Result<(), Error> {
        for c in s.bytes() {
            self.putc(c);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($args:tt)+) => ({
        use core::fmt::Write;
        let _ = write!($crate::debug::Uart {}, $($args)+);
    });
}

#[macro_export]
macro_rules! println {
    () => ({
        $crate::print!("\r\n")
    });
    ($fmt:expr) => ({
        $crate::print!(concat!($fmt, "\r\n"))
    });
    ($fmt:expr, $($args:tt)+) => ({
        $crate::print!(concat!($fmt, "\r\n"), $($args)+)
    });
}

/// Route the console pins to UART2 and set the baud rate. Returns the handle so
/// the caller can push the first message out synchronously.
pub fn setup_tx(perclk: u32) -> udma::Uart {
    let iox = Iox::new(utra::iox::HW_IOX_BASE as *mut u32);
    let udma_global = GlobalConfig::new();

    iox.set_alternate_function(IoxPort::PB, 13, IoxFunction::AF1);
    iox.set_alternate_function(IoxPort::PB, 14, IoxFunction::AF1);
    // rx as input, with pull-up (left configured so the pin isn't floating)
    iox.set_gpio_dir(IoxPort::PB, 13, IoxDir::Input);
    iox.set_gpio_pullup(IoxPort::PB, 13, IoxEnable::Enable);
    // tx as output
    iox.set_gpio_dir(IoxPort::PB, 14, IoxDir::Output);

    udma_global.clock_on(PeriphId::Uart2);
    udma_global.map_event(
        PeriphId::Uart2,
        PeriphEventType::Uart(EventUartOffset::Tx),
        EventChannel::Channel1,
    );

    // The address of the UART buffer is "hard-allocated" at an offset one page from
    // the top of IFRAM0. This is a convention the UDMA UART library relies on.
    // safety: safe to call, because the clock and events are set up above.
    let udma_uart = unsafe {
        udma::Uart::get_handle(
            utra::udma_uart_2::HW_UDMA_UART_2_BASE,
            crate::platform::UART_IFRAM_ADDR,
            crate::platform::UART_IFRAM_ADDR,
        )
    };
    udma_uart.set_baud(crate::UART_BAUD, perclk);
    udma_uart
}
