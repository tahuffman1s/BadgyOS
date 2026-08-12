#!/usr/bin/env python3
"""
badgeflash -- wait for a DEF CON 34 badge to appear in update mode, then push UF2s.

The badge's boot1 exposes a small emulated FAT volume labelled BAOCHIP when you
hold a button while powering on. Copying a .uf2 onto it writes ReRAM (or off-chip
swap flash); pressing a button then commits and boots.

This script:
  1. polls until that volume shows up (optionally mounting it for you),
  2. validates every UF2 block *before* writing anything,
  3. copies, fsyncs, and optionally unmounts so the write actually lands.

Step 2 matters. boot1's UF2 receiver silently drops any block whose family ID or
target address it does not like (bao1x-boot/boot1/src/platform/bao1x/usb/
handlers.rs:249) -- no error, no log, you just get a half-written image that
fails its signature check. Checking client-side turns that into a clear message.

Linux only (this is where the repo lives); stdlib only, no root required.

Examples
--------
    # wait for the badge, then flash the minimal firmware
    ./badgeflash.py ../target/riscv32imac-unknown-none-elf/release/badgyos.uf2

    # inspect the images without touching hardware
    ./badgeflash.py --dry-run ~/Documents/baoom/firmware/stock/*.uf2

    # restore stock (all three, in one session)
    ./badgeflash.py ~/Documents/baoom/firmware/stock/{loader,xous,swap}.uf2

    # sit in a loop and reflash every time the badge reappears
    ./badgeflash.py --watch badgyos.uf2
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import struct
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------
# Constants mirrored from the firmware tree. Sources given so they can be
# re-checked if upstream moves.
# ---------------------------------------------------------------------------

# libs/bao1x-api/src/lib.rs:31
UF2_FAMILY_BAO1X = 0xA7D7_6373

# bao1x-boot/boot1/src/platform/bao1x/usb/fat32_base.rs, root dir entry @0x410000.
# ALTCHIP is the alt-bootloader volume used when reflashing boot1 itself.
VOLUME_LABELS = ("BAOCHIP", "ALTCHIP")

# bao1x-boot/boot1/src/platform/bao1x/usb/mod.rs:153-154
USB_VID, USB_PID = 0x1D50, 0x6196

# Address windows boot1 will actually program. Anything else is dropped silently.
#   ReRAM: BAREMETAL_START(==LOADER_START) ..= HW_RERAM_MEM + RRAM_STORAGE_LEN
#   Swap:  SWAP_START_UF2 ..< SWAP_START_UF2 + SWAP_UF2_LEN
# (offsets/common.rs:10-21, handlers.rs:236-270)
RERAM_START, RERAM_END = 0x6006_0000, 0x603D_A000
SWAP_START, SWAP_END = 0x7000_0000, 0x7800_0000

# UF2 block layout
UF2_BLOCK_SIZE = 512
UF2_MAGIC0, UF2_MAGIC1, UF2_MAGIC_END = 0x0A32_4655, 0x9E5D_5157, 0x0AB1_6F30
UF2_FLAG_FAMILY_ID = 0x0000_2000
UF2_FLAG_NOT_MAIN_FLASH = 0x0000_0001
UF2_MAX_PAYLOAD = 476

# Signed-image header (libs/bao1x-api/src/signatures.rs). UNSIGNED_LEN is the
# offset of SealedFields inside SignatureInFlash:
#   u32 jal + 64B sig + u32 + 60B aad = 132  (signatures.rs:128-131)
SIG_UNSIGNED_LEN = 132
# Offset of pq_enabled *within* SealedFields, which is repr(C) with no padding:
#   version 0, magic 4, signed_len 12, function_code 16, anti_rollback 20,
#   min_semver 24, semver 40, pubkeys[4] (36B each) 56, toolchain 200,
#   corrected_version 220, pq_enabled 224
# Verified empirically: reads 0xA0A0_5555 on stock loader/xous, 0 on unsigned-PQ builds.
SEALED_PQ_ENABLED_OFF = 224
# signatures.rs:18 -- SLH-DSA sig, post-pended to the blob rather than in the header
SIGNATURE_PQ_LENGTH = 3856
FUNCTION_CODES = {
    0: "Invalid", 1: "Boot0", 2: "Boot1", 3: "UpdatedBoot1",
    4: "Loader", 5: "UpdatedLoader", 6: "Baremetal", 7: "UpdatedBaremetal",
    0x100: "Kernel", 0x101: "UpdatedKernel",
    0x8000: "Swap", 0x8001: "UpdatedSwap",
    0x10_0000: "App", 0x10_0001: "UpdatedApp",
}

IS_TTY = sys.stdout.isatty()

# Python block-buffers stdout when it is not a terminal, which would hide all
# progress when this is piped, logged, or run in the background -- exactly the
# cases where you are watching a log to see whether the badge showed up.
for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(line_buffering=True)
    except (AttributeError, OSError):  # pre-3.7, or a stream that cannot be reconfigured
        pass


def c(code: str, text: str) -> str:
    return f"\033[{code}m{text}\033[0m" if IS_TTY else text


def info(msg: str) -> None:
    print(f"{c('36', '::')} {msg}")


def warn(msg: str) -> None:
    print(f"{c('33', '!!')} {msg}")


def err(msg: str) -> None:
    print(f"{c('31', 'xx')} {msg}", file=sys.stderr)


def ok(msg: str) -> None:
    print(f"{c('32', 'ok')} {msg}")


# ---------------------------------------------------------------------------
# UF2 parsing / validation
# ---------------------------------------------------------------------------


@dataclass
class Uf2Info:
    path: Path
    blocks: int = 0
    lo: int = 0
    hi: int = 0
    payload_bytes: int = 0
    families: set = field(default_factory=set)
    problems: list = field(default_factory=list)
    region: str = "?"
    sig: dict | None = None

    @property
    def is_valid(self) -> bool:
        return not self.problems


def parse_uf2(path: Path) -> Uf2Info:
    """Parse and validate a UF2, reporting every reason boot1 might reject it."""
    nfo = Uf2Info(path=path)
    raw = path.read_bytes()

    if not raw:
        nfo.problems.append("file is empty")
        return nfo
    if len(raw) % UF2_BLOCK_SIZE:
        nfo.problems.append(
            f"size {len(raw)} is not a multiple of {UF2_BLOCK_SIZE}; not a UF2?"
        )
        return nfo

    total = len(raw) // UF2_BLOCK_SIZE
    addr_lo, addr_hi = None, None
    declared_total = None
    # Reassemble the payload so we can read the signature header out of it.
    image: dict[int, bytes] = {}

    for i in range(total):
        blk = raw[i * UF2_BLOCK_SIZE : (i + 1) * UF2_BLOCK_SIZE]
        m0, m1, flags, addr, plen, blkno, nblk, famid = struct.unpack("<8I", blk[:32])
        mend = struct.unpack("<I", blk[508:512])[0]

        if m0 != UF2_MAGIC0 or m1 != UF2_MAGIC1 or mend != UF2_MAGIC_END:
            nfo.problems.append(f"block {i}: bad UF2 magic")
            break
        if plen > UF2_MAX_PAYLOAD:
            nfo.problems.append(f"block {i}: payload {plen} > {UF2_MAX_PAYLOAD}")
            break
        if flags & UF2_FLAG_NOT_MAIN_FLASH:
            continue  # by spec, ignore these entirely
        if blkno != i:
            nfo.problems.append(f"block {i}: blockNo says {blkno}")
        if declared_total is None:
            declared_total = nblk
        elif nblk != declared_total:
            nfo.problems.append(f"block {i}: numBlocks {nblk} != {declared_total}")

        if flags & UF2_FLAG_FAMILY_ID:
            nfo.families.add(famid)
        else:
            nfo.families.add(None)

        nfo.blocks += 1
        nfo.payload_bytes += plen
        addr_lo = addr if addr_lo is None else min(addr_lo, addr)
        addr_hi = max(addr_hi or 0, addr + plen)
        image[addr] = blk[32 : 32 + plen]

    nfo.lo, nfo.hi = addr_lo or 0, addr_hi or 0

    if declared_total is not None and declared_total != total:
        nfo.problems.append(
            f"header says {declared_total} blocks, file holds {total}"
        )

    # --- the two checks boot1 makes, and fails silently on ---
    bad_family = {f for f in nfo.families if f != UF2_FAMILY_BAO1X}
    if bad_family:
        shown = ", ".join("none" if f is None else f"{f:#010x}" for f in sorted(
            bad_family, key=lambda x: -1 if x is None else x))
        nfo.problems.append(
            f"family ID {shown} != bao1x {UF2_FAMILY_BAO1X:#010x} "
            "-- boot1 would ignore these blocks"
        )

    if addr_lo is not None:
        if RERAM_START <= addr_lo and addr_hi <= RERAM_END:
            nfo.region = "ReRAM (on-chip)"
        elif SWAP_START <= addr_lo and addr_hi <= SWAP_END:
            nfo.region = "swap (off-chip SPI flash)"
        else:
            nfo.region = "OUT OF RANGE"
            nfo.problems.append(
                f"targets {addr_lo:#x}..{addr_hi:#x}, outside both windows "
                f"({RERAM_START:#x}..{RERAM_END:#x} ReRAM, "
                f"{SWAP_START:#x}..{SWAP_END:#x} swap) -- boot1 would drop it"
            )

    # --- decode the signature header, when this is a ReRAM image ---
    if addr_lo is not None and nfo.region.startswith("ReRAM"):
        flat = bytearray()
        for a in sorted(image):
            if a - addr_lo == len(flat):
                flat += image[a]
            else:
                break  # sparse; give up on header decode
        if len(flat) >= SIG_UNSIGNED_LEN + SEALED_PQ_ENABLED_OFF + 4:
            base = SIG_UNSIGNED_LEN
            (_ver, _m0, _m1, signed_len, fc, arb) = struct.unpack(
                "<6I", flat[base : base + 24]
            )
            minsem = flat[base + 24 : base + 40]
            semver = flat[base + 40 : base + 56]
            pq_enabled = struct.unpack(
                "<I", flat[base + SEALED_PQ_ENABLED_OFF : base + SEALED_PQ_ENABLED_OFF + 4]
            )[0]

            # Two sources of legitimate slack past the signed region:
            #  - an SLH-DSA signature is too big for the header, so it is
            #    post-pended to the blob (signatures.rs:16-18)
            #  - bin_to_uf2 emits ceil(len/256) 256-byte blocks, zero-padding the
            #    last one (sign_image.rs:585-600)
            # So only a *short* payload means real truncation.
            tail = SIGNATURE_PQ_LENGTH if pq_enabled else 0
            signed_total = SIG_UNSIGNED_LEN + signed_len + tail
            pad = len(flat) - signed_total
            nfo.sig = {
                "function": FUNCTION_CODES.get(fc, f"unknown({fc})"),
                "function_code": fc,
                "anti_rollback": arb,
                "signed_len": signed_len,
                "pq": bool(pq_enabled),
                "pad": pad,
                "covers_file": 0 <= pad < 256,
                "semver": _semver(semver),
                "min_semver": _semver(minsem),
            }
            if pad < 0:
                nfo.problems.append(
                    f"signature (+{tail}B PQ) needs {signed_total} bytes but the "
                    f"UF2 only carries {len(flat)} -- image is truncated"
                )
    return nfo


def _semver(b: bytes) -> str:
    """Decode xous-semver's 16-byte form (xous-semver-0.1.6/src/lib.rs:131).

    u16 maj, min, rev, extra (LE) | u32 commit | u32 "has commit" flag.
    """
    if len(b) < 16 or not any(b):
        return "-"
    maj, mnr, rev, extra = struct.unpack("<4H", b[0:8])
    commit = struct.unpack("<I", b[8:12])[0]
    has_commit = struct.unpack("<I", b[12:16])[0]
    s = f"v{maj}.{mnr}.{rev}-{extra}"
    if has_commit:
        s += f"-g{commit:x}"
    return s


def describe(nfo: Uf2Info) -> None:
    print(f"  {c('1', nfo.path.name)}")
    print(
        f"    {nfo.blocks} blocks, {nfo.payload_bytes} bytes payload -> "
        f"{nfo.lo:#010x}..{nfo.hi:#010x}  [{nfo.region}]"
    )
    if nfo.sig:
        s = nfo.sig
        cover = (f"covers image (+{s['pad']}B pad)"
                 if s["covers_file"] else c("33", "LENGTH MISMATCH"))
        print(
            f"    signed: {s['function']} (code {s['function_code']}), "
            f"anti-rollback {s['anti_rollback']}, "
            f"{'ed25519+PQ' if s['pq'] else 'ed25519 only'}, {cover}"
        )
        if s["semver"] != "-":
            print(f"    version: {s['semver']} (min {s['min_semver']})")
    for p in nfo.problems:
        print(f"    {c('31', 'PROBLEM')}: {p}")


# ---------------------------------------------------------------------------
# Finding the badge
# ---------------------------------------------------------------------------


def _unescape_mount(p: str) -> str:
    return re.sub(r"\\(\d{3})", lambda m: chr(int(m.group(1), 8)), p)


def mounted_volumes(labels=VOLUME_LABELS) -> list[Path]:
    """Mounted FAT volumes that look like a badge, newest-looking first."""
    found = []
    try:
        for line in Path("/proc/mounts").read_text().splitlines():
            parts = line.split()
            if len(parts) < 3:
                continue
            dev, mnt, fstype = parts[0], _unescape_mount(parts[1]), parts[2]
            if fstype not in ("vfat", "msdos", "exfat"):
                continue
            if Path(mnt).name.upper() in labels:
                found.append(Path(mnt))
    except OSError:
        pass

    # Also catch the case where the mountpoint was renamed but the label matches.
    for label in labels:
        link = Path("/dev/disk/by-label") / label
        if link.exists():
            dev = os.path.realpath(link)
            for m in _mountpoints_for(dev):
                if m not in found:
                    found.append(m)
    return found


def _mountpoints_for(dev: str) -> list[Path]:
    out = []
    try:
        for line in Path("/proc/mounts").read_text().splitlines():
            parts = line.split()
            if len(parts) >= 2 and parts[0] == dev:
                out.append(Path(_unescape_mount(parts[1])))
    except OSError:
        pass
    return out


def unmounted_devices(labels=VOLUME_LABELS) -> list[str]:
    """Badge partitions present but not mounted."""
    devs = []
    for label in labels:
        link = Path("/dev/disk/by-label") / label
        if link.exists():
            dev = os.path.realpath(link)
            if not _mountpoints_for(dev):
                devs.append(dev)
    return devs


def usb_present() -> bool:
    """True if the boot1 USB device is enumerated, mounted or not."""
    try:
        for d in Path("/sys/bus/usb/devices").iterdir():
            vid, pid = d / "idVendor", d / "idProduct"
            if vid.exists() and pid.exists():
                if (int(vid.read_text(), 16) == USB_VID
                        and int(pid.read_text(), 16) == USB_PID):
                    return True
    except OSError:
        pass
    return False


def try_mount(dev: str) -> Path | None:
    """Mount via udisksctl (no root needed on a normal desktop session)."""
    if not shutil.which("udisksctl"):
        return None
    try:
        r = subprocess.run(
            ["udisksctl", "mount", "-b", dev],
            capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    m = re.search(r"at (.+?)\.?$", (r.stdout or "").strip())
    if r.returncode == 0 and m:
        return Path(m.group(1))
    for mp in _mountpoints_for(dev):  # "already mounted"
        return mp
    return None


def wait_for_badge(timeout: float | None, automount: bool, poll: float = 0.5) -> Path:
    deadline = None if timeout is None else time.monotonic() + timeout
    spinner, tick, said_usb = "|/-\\", 0, False

    info("waiting for the badge in update mode "
         "(hold a button while plugging in / pressing reset)")
    print(f"   looking for a volume labelled {' or '.join(VOLUME_LABELS)}"
          f", or USB {USB_VID:04x}:{USB_PID:04x}")

    while True:
        vols = mounted_volumes()
        if vols:
            if IS_TTY:
                print("\r" + " " * 70 + "\r", end="")
            return vols[0]

        if automount:
            for dev in unmounted_devices():
                info(f"found {dev} unmounted; mounting")
                mp = try_mount(dev)
                if mp:
                    return mp
                warn(f"could not mount {dev} automatically "
                     f"-- mount it and re-run, or pass --dest")

        if not said_usb and usb_present():
            said_usb = True
            info("badge USB detected; waiting for the volume to mount")

        if deadline is not None and time.monotonic() > deadline:
            raise TimeoutError("timed out waiting for the badge")

        if IS_TTY:
            print(f"\r   {spinner[tick % 4]} waiting...", end="", flush=True)
        tick += 1
        time.sleep(poll)


def wait_for_removal(dest: Path, poll: float = 1.0) -> None:
    while dest in mounted_volumes():
        time.sleep(poll)


# ---------------------------------------------------------------------------
# Pushing
# ---------------------------------------------------------------------------


def copy_uf2(src: Path, dest_dir: Path) -> None:
    """Copy with an explicit fsync -- a plain copy can sit in page cache."""
    dst = dest_dir / src.name
    size = src.stat().st_size
    done = 0
    with open(src, "rb") as fin, open(dst, "wb") as fout:
        while chunk := fin.read(64 * 1024):
            fout.write(chunk)
            done += len(chunk)
            if IS_TTY:
                pct = done * 100 // max(size, 1)
                bar = "#" * (pct // 4)
                print(f"\r   {src.name}: [{bar:<25}] {pct:3d}%", end="", flush=True)
        fout.flush()
        os.fsync(fout.fileno())
    if IS_TTY:
        print(f"\r   {src.name}: [{'#' * 25}] 100%")


def eject(dest: Path) -> bool:
    """Unmount `dest`. False if it is not a mount point, or udisks refused."""
    if not shutil.which("udisksctl"):
        return False
    dev = None
    try:
        for line in Path("/proc/mounts").read_text().splitlines():
            parts = line.split()
            if len(parts) >= 2 and _unescape_mount(parts[1]) == str(dest):
                dev = parts[0]
                break
    except OSError:
        return False
    if not dev:
        return False
    r = subprocess.run(["udisksctl", "unmount", "-b", dev],
                       capture_output=True, text=True)
    return r.returncode == 0


# ---------------------------------------------------------------------------


DEFAULT_UF2 = (
    Path(__file__).resolve().parent.parent
    / "target/riscv32imac-unknown-none-elf/release/badgyos.uf2"
)

WARNING = """\
Flashing a developer-signed image is IRREVERSIBLE.

boot1 will erase the badge's secret keys and increment the DEVELOPER_MODE
one-way counter. k0 -- the DEF CON light-encryption key -- is destroyed and
cannot be recovered, not even by restoring stock firmware. The badge is out of
the conference light-breeding game permanently, and is flagged as a developer
device forever.

Restoring stock is otherwise fine: anti-rollback counters are per function code,
so this cannot block reflashing loader/xous/swap.

If you have not already: enter the boot1 console (PB14/PB13, 1000000 8N1), run
`audit`, and check the boot1 row / key3 column. If it reads `revoked`, a
developer-signed image will not boot at all."""


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Wait for a DC34 badge in update mode, then push UF2 firmware.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("files", nargs="*", type=Path,
                    help=f"UF2 files (default: {DEFAULT_UF2.name} from the build tree)")
    ap.add_argument("--dest", type=Path,
                    help="skip detection, write to this mounted directory")
    ap.add_argument("--timeout", type=float, default=None,
                    help="give up after N seconds (default: wait forever)")
    ap.add_argument("--watch", action="store_true",
                    help="after flashing, wait for the badge again and repeat")
    ap.add_argument("--dry-run", action="store_true",
                    help="validate the UF2s and exit; touches no hardware")
    ap.add_argument("--yes", "-y", action="store_true",
                    help="skip the irreversibility confirmation")
    ap.add_argument("--eject", action="store_true",
                    help="unmount the volume after copying")
    ap.add_argument("--no-automount", action="store_true",
                    help="do not try to mount the badge via udisksctl")
    ap.add_argument("--force", action="store_true",
                    help="flash even if validation found problems (not advised)")
    args = ap.parse_args()

    if args.watch and args.dest:
        # --dest is a plain directory, so it never shows up as a mounted volume;
        # wait_for_removal() would return instantly and we would spin, recopying.
        err("--watch needs volume detection; it cannot be combined with --dest")
        return 2

    files = args.files or ([DEFAULT_UF2] if DEFAULT_UF2.exists() else [])
    if not files:
        err("no UF2 given, and the default build output does not exist")
        err(f"  expected: {DEFAULT_UF2}")
        err("  build it with: ./build.sh")
        return 2

    missing = [f for f in files if not f.is_file()]
    if missing:
        for f in missing:
            err(f"not found: {f}")
        return 2

    # ---- validate first, always ----
    info(f"validating {len(files)} image(s)")
    infos = []
    for f in files:
        try:
            nfo = parse_uf2(f)
        except Exception as e:  # a corrupt file should not traceback
            err(f"{f}: could not parse: {e}")
            return 2
        infos.append(nfo)
        describe(nfo)

    broken = [n for n in infos if not n.is_valid]
    if broken:
        err(f"{len(broken)} image(s) failed validation (see PROBLEM lines above)")
        err("boot1 drops bad blocks silently, so this would look like a "
            "corrupt flash rather than an error")
        if not args.force:
            return 1
        warn("--force given; continuing anyway")

    if args.dry_run:
        ok("dry run: images look flashable")
        return 0

    # ---- confirm ----
    if not args.yes:
        print()
        print(c("33", WARNING))
        print()
        try:
            if input("Type 'flash' to continue: ").strip().lower() != "flash":
                info("aborted")
                return 1
        except (EOFError, KeyboardInterrupt):
            print()
            info("aborted")
            return 1

    # ---- wait + push ----
    first = True
    while True:
        if args.dest:
            dest = args.dest
            if not dest.is_dir():
                err(f"--dest is not a directory: {dest}")
                return 2
        else:
            if not first:
                info("waiting for the badge to come back")
            try:
                dest = wait_for_badge(args.timeout, automount=not args.no_automount)
            except TimeoutError as e:
                err(str(e))
                return 1
            except KeyboardInterrupt:
                print()
                info("cancelled")
                return 1

        ok(f"badge volume: {dest}")
        try:
            for f in files:
                copy_uf2(f, dest)
            os.sync()
        except OSError as e:
            err(f"copy failed: {e}")
            err("if this says 'No space left', the image is larger than the "
                "emulated volume -- check the target region")
            return 1

        if args.eject:
            if eject(dest):
                info("volume unmounted; safe to unplug")
            else:
                warn("could not unmount (not a mount point, or udisks refused); "
                     "data was fsync'd regardless")

        ok(f"pushed {len(files)} image(s). Press any button on the badge to "
           "commit and boot.")

        if not args.watch:
            return 0
        first = False
        try:
            wait_for_removal(dest)
        except KeyboardInterrupt:
            print()
            info("done")
            return 0


if __name__ == "__main__":
    sys.exit(main())
