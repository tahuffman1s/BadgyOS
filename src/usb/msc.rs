//! Bulk-Only Transport and the slice of SCSI a host needs to mount a drive.
//!
//! Ported from `boot1/src/platform/bao1x/usb/handlers.rs`, with three
//! deliberate departures:
//!
//! * **The disk is real.** boot1 never stores what the host writes -- it sniffs the sector stream for UF2
//!   magic and throws the rest away, because it only ever needs to recognize a firmware image. We need to
//!   hand a filename and a file body back to the user, which is not recoverable from a bare data sector, so
//!   every written sector lands in [`RAMDISK`] and is served back on the next read. That also means the
//!   host's cache and ours agree, which is what stops a host from deciding the volume is damaged and
//!   "repairing" it.
//!
//! * **The LBA is 32 bits.** boot1 reads only `cdb[4..6]`, so its 128 MiB volume aliases every 32 MiB. Ours
//!   is 512 KiB and would never notice, but the field is 32 bits wide and there is no reason to write it
//!   wrong.
//!
//! * **Removable media, and a flush signal.** `INQUIRY` reports removable so the host offers an eject, and
//!   `SYNCHRONIZE CACHE` and the unlock half of `PREVENT ALLOW MEDIUM REMOVAL` are handled rather than
//!   swallowed -- those are the two moments a host tells us it has finished writing.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bao1x_hal::usb::driver::*;

use super::proto;

/// Bytes in the emulated disk. Also the size of the FAT12 volume on it -- the
/// two are the same thing, there is no partition table.
pub const DISK_BYTES: usize = badgy_fat::VOLUME_BYTES;
pub const SECTOR_SIZE: usize = 512;
pub const SECTOR_COUNT: usize = DISK_BYTES / SECTOR_SIZE;

/// The disk itself, plus one sector of slack.
///
/// Zero-initialized on purpose. `.bss` is NOBITS, so half a megabyte of disk
/// costs nothing in the flashed image and, more importantly, needs no entries
/// in the image builder's 40-slot poke table -- a non-zero initializer here
/// would blow that limit instantly. `early_init` clears the whole region before
/// anything runs.
///
/// The slack sector exists to work around an off-by-one in the HAL. Its
/// `setup_big_read` copies real data only when `offset + len < disk.len()` and
/// otherwise zero-fills, so a read ending *exactly* at the end of the disk --
/// which is what a host does when it probes the last sector to confirm the
/// reported capacity -- would come back as zeros. boot1 never notices because
/// its disk is a small window inside a much larger advertised volume, where the
/// tail is synthetic anyway. Handing the HAL a slice that is one sector longer
/// than anything addressable makes the comparison come out right.
static mut RAMDISK: [u8; DISK_BYTES + SECTOR_SIZE] = [0; DISK_BYTES + SECTOR_SIZE];

/// Staging buffer for bulk transfers, in IFRAM1.
///
/// IFRAM1 is otherwise unused on this board -- it belongs to the camera, which
/// this firmware never powers up. Keeping the staging buffer out of IFRAM0
/// removes any chance of overlapping the UDC's own structures, the display's
/// SPI buffers or the console UART's.
const APP_BUF_ADDR: usize = utralib::HW_IFRAM1_MEM;
const APP_BUF_LEN: usize = 4096 * 2;

/// Where the driver expects the command and status blocks to live, inside the
/// UDC's IFRAM window.
fn cbw_addr() -> usize { bao1x_hal::board::CRG_UDC_MEMBASE + CRG_UDC_APP_BUFOFFSET }
fn csw_addr() -> usize { cbw_addr() + CRG_UDC_APP_BUF_LEN }
fn ep1_in_addr() -> usize { cbw_addr() + 1024 }
const EP1_IN_LEN: usize = 1024;

/// Bumped on every completed write. The importer watches it to notice that
/// something changed without having to understand the write stream.
pub static WRITE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Set when the host says it has finished: `SYNCHRONIZE CACHE`, or the unlock
/// half of `PREVENT ALLOW MEDIUM REMOVAL` (which is what eject sends).
pub static FLUSH_REQUESTED: AtomicBool = AtomicBool::new(false);
/// True once the host has configured the interface, i.e. the drive is mounted
/// as far as we can tell.
pub static CONFIGURED: AtomicBool = AtomicBool::new(false);

/// The addressable disk: exactly the bytes the FAT volume occupies, and exactly
/// what the reported capacity covers.
///
/// # Safety
///
/// Single-threaded firmware with no interrupts: the only code that touches the
/// disk is this module (from the poll loop) and the importer (also from the poll
/// loop, between polls). There is no concurrent access to race with.
pub fn disk() -> &'static mut [u8] { &mut backing()[..DISK_BYTES] }

/// The whole array including the slack sector. Only the read path wants this --
/// see the note on [`RAMDISK`].
fn backing() -> &'static mut [u8] {
    // safety: `RAMDISK` is a plain byte array with no interior invariants, and
    // nothing else in the firmware aliases it.
    unsafe { &mut *core::ptr::addr_of_mut!(RAMDISK) }
}

fn app_buf() -> &'static mut [u8] {
    // safety: IFRAM1 is unclaimed on this board and this is its only user.
    unsafe { core::slice::from_raw_parts_mut(APP_BUF_ADDR as *mut u8, APP_BUF_LEN) }
}

// -------------------------------------------------------------- BOT structures

const CBW_SIGNATURE: u32 = 0x4342_5355; // 'USBC'
const CSW_SIGNATURE: u32 = 0x5342_5355; // 'USBS'
const CBW_LEN: usize = 31;

#[derive(Default, Clone, Copy)]
struct Cbw {
    signature: u32,
    tag: u32,
    data_transfer_length: u32,
    flags: u8,
    _lun: u8,
    _cdb_length: u8,
    cdb: [u8; 16],
}

impl Cbw {
    fn parse(b: &[u8]) -> Option<Cbw> {
        if b.len() < CBW_LEN {
            return None;
        }
        let mut cdb = [0u8; 16];
        cdb.copy_from_slice(&b[15..31]);
        Some(Cbw {
            signature: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            tag: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            data_transfer_length: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            flags: b[12],
            _lun: b[13],
            _cdb_length: b[14],
            cdb,
        })
    }

    /// True when the host expects data to come back from the device.
    fn is_read(&self) -> bool { self.flags & 0x80 != 0 }

    /// The 32-bit LBA of a READ(10)/WRITE(10).
    fn lba10(&self) -> u32 { u32::from_be_bytes([self.cdb[2], self.cdb[3], self.cdb[4], self.cdb[5]]) }

    /// Transfer length in blocks, for a 10-byte command.
    fn blocks10(&self) -> u32 { u16::from_be_bytes([self.cdb[7], self.cdb[8]]) as u32 }
}

/// The status block, which lives at a fixed address the hardware sends from.
///
/// It is *staged* when a command is decoded and *sent* when the data phase
/// finishes, because that is the order the transport requires: the residue and
/// status are known up front, but they must not go on the wire until the data
/// has. Sending is therefore a separate step that does not rebuild the block --
/// rebuilding it is how a correct residue turns into a zero and a host ends up
/// believing it read more than it did.
struct Csw;

impl Csw {
    /// Write the status block without sending it.
    fn stage(tag: u32, residue: u32, status: u8) {
        // safety: `csw_addr()` is inside the UDC's IFRAM allocation, which the
        // driver reserved when it was constructed.
        let buf = unsafe { core::slice::from_raw_parts_mut(csw_addr() as *mut u8, 13) };
        buf[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        buf[4..8].copy_from_slice(&tag.to_le_bytes());
        buf[8..12].copy_from_slice(&residue.to_le_bytes());
        buf[12] = status;
    }

    /// Put whatever is currently staged on the wire.
    fn send_staged(usb: &mut CorigineUsb) {
        usb.bulk_xfer(1, USB_SEND, csw_addr(), 13, 0, 0);
        usb.ms_state = UmsState::StatusPhase;
    }

    /// For commands with no data phase: stage and send in one go.
    fn send_now(usb: &mut CorigineUsb, tag: u32, residue: u32, status: u8) {
        Self::stage(tag, residue, status);
        Self::send_staged(usb);
    }
}

const STATUS_GOOD: u8 = 0;
const STATUS_CHECK_CONDITION: u8 = 1;

/// Wait for the next command block.
fn rearm_command(usb: &mut CorigineUsb) {
    usb.ms_state = UmsState::CommandPhase;
    let cbw = usb.cbw_ptr();
    usb.bulk_xfer(1, USB_RECV, cbw, CBW_LEN, 0, 0);
}

/// Fail a command, and still move the data the host is expecting to move.
///
/// The transport's own answer is to stall the data endpoint and let the host
/// clear it, but stalling is off the table (see `bulk_out_complete`). Satisfying
/// the data phase -- zeros outward, a sink inward -- keeps both ends in step
/// without a stall, and the CHECK CONDITION still tells the host it failed.
fn fail(usb: &mut CorigineUsb, cbw: Cbw, key: u8, asc: u8, ascq: u8) {
    set_sense(key, asc, ascq);
    let want = cbw.data_transfer_length;
    if want == 0 {
        Csw::send_now(usb, cbw.tag, 0, STATUS_CHECK_CONDITION);
        return;
    }
    Csw::stage(cbw.tag, want, STATUS_CHECK_CONDITION);
    if cbw.is_read() {
        let take = (want as usize).min(EP1_IN_LEN);
        // safety: `ep1_in_addr()` is inside the UDC IFRAM window and `take` is
        // clamped to that buffer.
        let buf = unsafe { core::slice::from_raw_parts_mut(ep1_in_addr() as *mut u8, EP1_IN_LEN) };
        buf[..take].fill(0);
        usb.bulk_xfer(1, USB_SEND, ep1_in_addr(), take, 0, 0);
    } else {
        // Aim the write at an offset past the end of the disk: the copy in
        // `bulk_out_complete` is bounds-checked and will drop it.
        usb.setup_big_write(APP_BUF_ADDR, APP_BUF_LEN, DISK_BYTES, want as usize);
    }
    usb.ms_state = UmsState::DataPhase;
}

/// Sense data for the last failed command, in fixed format.
struct Sense {
    key: u8,
    asc: u8,
    ascq: u8,
}

static mut PENDING_SENSE: Option<Sense> = None;

fn set_sense(key: u8, asc: u8, ascq: u8) {
    // safety: single-threaded, no interrupts, so no other reference can exist.
    unsafe { core::ptr::addr_of_mut!(PENDING_SENSE).write(Some(Sense { key, asc, ascq })) };
}

fn take_sense() -> Sense {
    // safety: as above. `addr_of_mut!` rather than a `&mut` to the static, so
    // no reference to it is ever materialized.
    unsafe { (*core::ptr::addr_of_mut!(PENDING_SENSE)).take() }.unwrap_or(Sense { key: 0, asc: 0, ascq: 0 })
}

// ------------------------------------------------------------------ lifecycle

/// Called once the host has selected the configuration, and again after a
/// Bulk-Only reset. Arms the endpoints and waits for the first command.
pub fn on_configured(usb: &mut CorigineUsb) {
    let mps = match usb.get_speed() {
        PortSpeed::Fs => proto::FS_BULK_MPS,
        _ => proto::HS_BULK_MPS,
    };
    usb.ep_enable(1, USB_RECV, mps as u16, EpType::BulkOutbound);
    usb.ep_enable(1, USB_SEND, mps as u16, EpType::BulkInbound);
    usb.assign_completion_handler(bulk_in_complete, 1, USB_SEND);
    usb.assign_completion_handler(bulk_out_complete, 1, USB_RECV);

    // Drop any half-finished transfer. `on_configured` also runs on a Bulk-Only
    // reset, which is exactly the case where a transfer was interrupted -- and a
    // stale `remaining_rd` would make the first completion after the reset
    // resume the *old* command's chunking.
    usb.remaining_rd = None;
    usb.remaining_wr = None;
    usb.callback_wr = None;

    rearm_command(usb);
    CONFIGURED.store(true, Ordering::SeqCst);
}

/// The host dropped the configuration. Stop claiming the drive is mounted.
pub fn on_deconfigured() { CONFIGURED.store(false, Ordering::SeqCst); }

/// A bulk IN transfer finished: either the data phase completed and the status
/// block is next, or the status block itself went out and the next command can
/// be received.
fn bulk_in_complete(usb: &mut CorigineUsb, _addr: usize, info: u32, _err: u8, _residual: u16) {
    let length = info & 0xFFFF;
    match usb.ms_state {
        UmsState::DataPhase => {
            // A read longer than the staging buffer is sent in chunks; keep
            // feeding until the driver says there is nothing left.
            if let Some((offset, remaining)) = usb.remaining_rd.take() {
                let buf = app_buf();
                let d = backing();
                usb.setup_big_read(buf, d, offset, remaining, None);
                return;
            }
            Csw::send_staged(usb);
        }
        UmsState::StatusPhase if length == 13 => rearm_command(usb),
        _ => (),
    }
}

/// A bulk OUT transfer finished: either a new command block arrived, or a chunk
/// of write data did.
fn bulk_out_complete(usb: &mut CorigineUsb, buf_addr: usize, info: u32, _err: u8, residual: u16) {
    // `info` carries the length the TRB was *programmed* with (`transfer_len` is
    // dw2 bits 17:0); `residual` is how much of it did not arrive. Comparing the
    // programmed length against 31 would be comparing a constant with itself,
    // and a short transfer would be taken at face value -- copying whatever
    // stale bytes were left in the staging buffer into the disk.
    //
    // This describes one TRB, not one transfer. See `bulk_out_complete`'s data
    // phase for why that distinction matters.
    let programmed = (info & 0x3_FFFF) as usize;
    let length = programmed.saturating_sub(residual as usize);

    match usb.ms_state {
        UmsState::CommandPhase => {
            let ok = length == CBW_LEN
                && super::ptr_in_udc_window(buf_addr, CBW_LEN)
                // safety: bounds-checked into the UDC's own buffer window.
                && match Cbw::parse(unsafe {
                    core::slice::from_raw_parts(buf_addr as *const u8, CBW_LEN)
                }) {
                    Some(cbw) if cbw.signature == CBW_SIGNATURE => {
                        dispatch(usb, cbw);
                        true
                    }
                    _ => false,
                };
            if !ok {
                // The transport says to stall both bulk endpoints here so the
                // host knows to send a Bulk-Only reset. We do not, because the
                // HAL's `ep_halt` busy-waits on `EPRUNNING_RUNNING != 0` -- a
                // 30-bit field covering every endpoint from PEI 2 up -- which
                // cannot clear while the *other* bulk endpoint is armed. It
                // would hang, and a hang on this device needs a power cycle.
                // boot1 has the same call and gets away with it only because
                // the path never fires against a real host.
                //
                // Re-arming instead leaves the host to time out and reset,
                // which our reset handler serves. Slower to recover, but it
                // does recover.
                crate::println!("USB: bad command block, re-arming");
                rearm_command(usb);
            }
        }

        UmsState::DataPhase => {
            // This is the part boot1 does not do: actually keep the data.
            if let Some((offset, len)) = usb.callback_wr.take() {
                // Only the bytes that actually arrived. A host that sends less
                // than it promised must not have the shortfall filled in with
                // the previous transfer's data.
                //
                // `length` is not that number on its own. `bulk_xfer` cuts any
                // transfer longer than `MAX_TRB_XFER_LEN` -- 1024 bytes -- into
                // a chain of TRBs and raises interrupt-on-complete on the last
                // one only, so the TRB this event describes is the tail of a
                // chain, not the whole transfer. Clamping to it kept the first
                // kilobyte of every write and dropped the rest, which reads
                // back later as a file whose head is right and whose tail is
                // whatever the volume held before: a host `cp` of a 3 KiB
                // script lands as 1 KiB of script followed by two stale
                // clusters, and the badge reports a syntax error in a file the
                // user can see is fine on their machine.
                //
                // The completing TRB's own data pointer is the missing piece.
                // It sits `n * MAX_TRB_XFER_LEN` bytes into the staging buffer,
                // so that distance plus the part of this TRB that did arrive is
                // the length of the whole chain. A transfer cut short still
                // under-counts rather than over-counts, which is what keeps
                // stale staging bytes out of the disk.
                let arrived = match buf_addr.checked_sub(APP_BUF_ADDR) {
                    Some(before) if before < APP_BUF_LEN => before + length,
                    // Not a pointer into the staging buffer at all, so nothing
                    // can be said about what arrived. Keep none of it.
                    _ => 0,
                };
                let len = len.min(arrived);
                let src = app_buf();
                let d = disk();
                let end = offset.saturating_add(len);
                if end <= d.len() && len <= src.len() {
                    d[offset..end].copy_from_slice(&src[..len]);
                    WRITE_COUNT.fetch_add(1, Ordering::SeqCst);
                }
                // Anything past the end of the disk is dropped. It should not
                // happen -- the capacity we report is the size of the array --
                // but a host that asks anyway must not corrupt memory.
            }
            if let Some((offset, remaining)) = usb.remaining_wr.take() {
                usb.setup_big_write(APP_BUF_ADDR, APP_BUF_LEN, offset, remaining);
                return;
            }
            Csw::send_staged(usb);
        }

        _ => (),
    }
}

// -------------------------------------------------------------------- SCSI

fn dispatch(usb: &mut CorigineUsb, cbw: Cbw) {
    match cbw.cdb[0] {
        0x00 => ok(usb, cbw),                     // TEST UNIT READY
        0x03 => request_sense(usb, cbw),          // REQUEST SENSE
        0x12 => inquiry(usb, cbw),                // INQUIRY
        0x1B => ok(usb, cbw),                     // START STOP UNIT
        0x1E => prevent_allow(usb, cbw),          // PREVENT ALLOW MEDIUM REMOVAL
        0x25 => read_capacity_10(usb, cbw),       // READ CAPACITY (10)
        0x28 => read10(usb, cbw),                 // READ (10)
        0x2A => write10(usb, cbw),                // WRITE (10)
        0x23 => read_format_capacities(usb, cbw), // READ FORMAT CAPACITIES
        0x35 => synchronize_cache(usb, cbw),      // SYNCHRONIZE CACHE (10)
        0x9E => read_capacity_16(usb, cbw),       // SERVICE ACTION IN / READ CAPACITY (16)
        // Everything else -- MODE SENSE, READ FORMAT CAPACITIES, VERIFY and
        // the rest -- is answered with success and no data. That is not
        // strictly correct SCSI (an unknown opcode should be CHECK CONDITION
        // with sense 05/20/00) but it is what boot1 ships and what every host
        // tested against it accepts. Returning errors here invites hosts to
        // retry, escalate, and eventually give up on the device.
        _ => ok(usb, cbw),
    }
}

/// Success with no data phase, still moving whatever the host expects to move.
///
/// A command answered without its data phase leaves the host waiting on an
/// endpoint that will never move, which reads as a wedged device. Both
/// directions have to be served even when there is nothing to say.
fn ok(usb: &mut CorigineUsb, cbw: Cbw) {
    let want = cbw.data_transfer_length;
    if want == 0 {
        Csw::send_now(usb, cbw.tag, 0, STATUS_GOOD);
        return;
    }
    Csw::stage(cbw.tag, want, STATUS_GOOD);
    if cbw.is_read() {
        // A zero-length IN completes the host's read with a short packet.
        usb.bulk_xfer(1, USB_SEND, ep1_in_addr(), 0, 0, 0);
    } else {
        // Accept and discard: past the end of the disk, so the bounds check in
        // `bulk_out_complete` drops it.
        usb.setup_big_write(APP_BUF_ADDR, APP_BUF_LEN, DISK_BYTES, want as usize);
    }
    usb.ms_state = UmsState::DataPhase;
}

/// Send `data` as the data phase, then let `bulk_in_complete` send the status.
fn send_data(usb: &mut CorigineUsb, cbw: Cbw, data: &[u8]) {
    if cbw.data_transfer_length == 0 || !cbw.is_read() {
        Csw::send_now(usb, cbw.tag, 0, STATUS_GOOD);
        return;
    }
    let n = data.len().min(cbw.data_transfer_length as usize).min(EP1_IN_LEN);
    // safety: `ep1_in_addr()` is inside the UDC IFRAM window reserved by the
    // driver, and `n` is clamped to that buffer's length.
    let buf = unsafe { core::slice::from_raw_parts_mut(ep1_in_addr() as *mut u8, EP1_IN_LEN) };
    buf[..n].copy_from_slice(&data[..n]);
    usb.bulk_xfer(1, USB_SEND, ep1_in_addr(), n, 0, 0);
    usb.ms_state = UmsState::DataPhase;
    Csw::stage(cbw.tag, cbw.data_transfer_length - n as u32, STATUS_GOOD);
}

fn request_sense(usb: &mut CorigineUsb, cbw: Cbw) {
    let s = take_sense();
    let mut d = [0u8; 18];
    // 0x70 = current error, fixed format. boot1 sends 18 zero bytes here,
    // which is a malformed response that hosts happen to tolerate.
    d[0] = 0x70;
    d[2] = s.key;
    d[7] = 10; // additional length
    d[12] = s.asc;
    d[13] = s.ascq;
    send_data(usb, cbw, &d);
}

fn inquiry(usb: &mut CorigineUsb, cbw: Cbw) {
    let mut d = [0u8; 36];
    d[0] = 0x00; // direct-access block device
    // Removable. boot1 reports fixed media; declaring removable is what makes
    // a host offer "eject", and eject is the clean signal that the user has
    // finished dropping files.
    d[1] = 0x80;
    d[2] = 0x00; // no claimed standard
    d[3] = 0x01; // response data format
    d[4] = 31; // additional length: 36 bytes total
    d[8..16].copy_from_slice(b"Baochip ");
    d[16..32].copy_from_slice(b"BadgyOS         ");
    d[32..36].copy_from_slice(b"0001");
    send_data(usb, cbw, &d);
}

fn prevent_allow(usb: &mut CorigineUsb, cbw: Cbw) {
    // cdb[4] bit 0: 1 = lock the media in, 0 = allow removal. The unlock is
    // what an eject sends, and it is the most reliable "I am done" a host
    // gives us. boot1 ignores this byte entirely.
    if cbw.cdb[4] & 1 == 0 {
        FLUSH_REQUESTED.store(true, Ordering::SeqCst);
    }
    ok(usb, cbw);
}

fn synchronize_cache(usb: &mut CorigineUsb, cbw: Cbw) {
    FLUSH_REQUESTED.store(true, Ordering::SeqCst);
    ok(usb, cbw);
}

fn read_capacity_10(usb: &mut CorigineUsb, cbw: Cbw) {
    let mut d = [0u8; 8];
    // The field is the address of the *last* block, not the count.
    d[..4].copy_from_slice(&((SECTOR_COUNT - 1) as u32).to_be_bytes());
    d[4..].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
    send_data(usb, cbw, &d);
}

/// Windows asks removable media for this before it will mount them, and an
/// empty answer is what produces "please insert a disk".
fn read_format_capacities(usb: &mut CorigineUsb, cbw: Cbw) {
    let mut d = [0u8; 12];
    d[3] = 8; // capacity list length: one descriptor
    d[4..8].copy_from_slice(&(SECTOR_COUNT as u32).to_be_bytes());
    d[8] = 0x02; // formatted media
    // Block length is a 24-bit field here, not 32.
    d[9..12].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes()[1..]);
    send_data(usb, cbw, &d);
}

fn read_capacity_16(usb: &mut CorigineUsb, cbw: Cbw) {
    let mut d = [0u8; 32];
    d[..8].copy_from_slice(&((SECTOR_COUNT - 1) as u64).to_be_bytes());
    d[8..12].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
    send_data(usb, cbw, &d);
}

/// Bounds-check a transfer against the disk, in bytes.
fn range_of(cbw: &Cbw) -> Option<(usize, usize)> {
    let lba = cbw.lba10() as usize;
    let blocks = cbw.blocks10() as usize;
    let offset = lba.checked_mul(SECTOR_SIZE)?;
    let len = blocks.checked_mul(SECTOR_SIZE)?;
    if offset.checked_add(len)? > DISK_BYTES {
        return None;
    }
    Some((offset, len))
}

fn read10(usb: &mut CorigineUsb, cbw: Cbw) {
    let Some((offset, len)) = range_of(&cbw) else {
        // 05/21/00 LOGICAL BLOCK ADDRESS OUT OF RANGE.
        fail(usb, cbw, 0x05, 0x21, 0x00);
        return;
    };
    // A READ whose command block says IN but whose CBW says OUT is a host bug;
    // 05/24/00 is INVALID FIELD IN CDB.
    if !cbw.is_read() && cbw.data_transfer_length > 0 {
        fail(usb, cbw, 0x05, 0x24, 0x00);
        return;
    }
    if cbw.data_transfer_length == 0 || len == 0 {
        Csw::send_now(usb, cbw.tag, cbw.data_transfer_length, STATUS_GOOD);
        return;
    }
    let len = len.min(cbw.data_transfer_length as usize);
    let residue = cbw.data_transfer_length - len as u32;

    let buf = app_buf();
    // `backing()`, not `disk()`: see the note on RAMDISK. The volume is fully
    // backed, so there is no overflow handler either -- every LBA a host can
    // name has real bytes behind it. boot1 needs one because it advertises
    // 128 MiB over a much smaller buffer.
    let d = backing();
    usb.setup_big_read(buf, d, offset, len, None);
    usb.ms_state = UmsState::DataPhase;
    Csw::stage(cbw.tag, residue, STATUS_GOOD);
}

fn write10(usb: &mut CorigineUsb, cbw: Cbw) {
    let Some((offset, len)) = range_of(&cbw) else {
        fail(usb, cbw, 0x05, 0x21, 0x00);
        return;
    };
    if cbw.is_read() && cbw.data_transfer_length > 0 {
        fail(usb, cbw, 0x05, 0x24, 0x00);
        return;
    }
    if cbw.data_transfer_length == 0 || len == 0 {
        Csw::send_now(usb, cbw.tag, cbw.data_transfer_length, STATUS_GOOD);
        return;
    }
    let len = len.min(cbw.data_transfer_length as usize);
    let residue = cbw.data_transfer_length - len as u32;

    usb.setup_big_write(APP_BUF_ADDR, APP_BUF_LEN, offset, len);
    usb.ms_state = UmsState::DataPhase;
    Csw::stage(cbw.tag, residue, STATUS_GOOD);
}
