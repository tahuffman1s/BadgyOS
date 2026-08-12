#!/usr/bin/env bash
#
# Build, package and sign BadgyOS into a flashable badgyos.uf2.
#
# This reproduces what `cargo xtask bao1x-baremetal-baosec` does inside xous-core,
# but for an out-of-tree crate -- that target hardcodes the crate name
# `baremetal`, and vendor/xous-core stays read-only:
#
#   1. cargo build   -> ELF, linked at 0x6006_0400 (see link.x)
#   2. xous-copy-object --bao1x  -> flat image + the 256-byte StaticsInRom header
#      that early_init() reads back to initialize .data/.bss
#   3. xous-sign-image --function-code baremetal -> prepends the 768-byte signature
#      block (whose first word is the jump boot1 takes) and emits the .uf2
#
# Signing uses xous-core's *developer* key, whose private half is public. See
# README.md: booting a developer-signed image is a one-way trip for the badge.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The board-support crates and the two host tools both live in the pinned
# checkout. bootstrap.sh puts one here; run it rather than failing with a
# cargo error about a missing path dependency.
if [[ ! -e "${HERE}/vendor/xous-core" ]]; then
    echo "==> 0/3 vendor/xous-core missing; running ./bootstrap.sh"
    "${HERE}/bootstrap.sh"
fi
XOUS_CORE="$(cd "${HERE}/vendor/xous-core" && pwd -P)"

TARGET=riscv32imac-unknown-none-elf
OUTDIR="${HERE}/target/${TARGET}/release"
ELF="${OUTDIR}/badgyos"
PRESIGN="${OUTDIR}/badgyos-presign.img"
IMG="${OUTDIR}/badgyos.img"

# Must match SIGBLOCK_LEN in src/platform.rs and the ORIGIN in link.x.
SIGBLOCK_LEN=768
# Anti-rollback floor. Matches MIN_XOUS_VERSION in xous-core/xtask/src/main.rs.
MIN_XOUS_VER=v0.9.8-791
SIGNING_KEY="${XOUS_CORE}/devkey/dev.key"

echo "==> 1/3 building ${TARGET} release"
# cd, because cargo discovers .cargo/config.toml from the working directory.
cd "${HERE}"
cargo build --release --target "${TARGET}"

echo "==> 2/3 copy-object (flat image + statics)"
# These are host tools living in the xous-core workspace; run them from there so
# they pick up that workspace's cargo config rather than ours.
cd "${XOUS_CORE}"
cargo run --release --package xous-tools --bin xous-copy-object -- \
    "${ELF}" "${PRESIGN}" --bao1x

# The statics header carries the two limits that are easy to blow through without
# noticing: the poke table (40 entries, one per non-zero word of .data) and the
# u16 offsets into the data segment. Both fail at *boot*, not at build time --
# `xous-copy-object` errors on the poke overflow but nothing warns as you
# approach it -- so print the numbers on every build.
python3 - "${PRESIGN}" "${HERE}/src/platform.rs" <<'PY'
import re, struct, sys
d = open(sys.argv[1], 'rb').read()
_jump, ver, pokes, origin, size = struct.unpack_from('<IHHII', d, 0)
heap = origin + size
RAM_TOP = 0x6120_0000
# Read the heap size out of the source so the two cannot drift apart.
m = re.search(r'HEAP_LEN: usize = (.+?);', open(sys.argv[2]).read())
HEAP_LEN = eval(m.group(1).replace('_', ''))
print(f"    statics v{ver}: pokes {pokes}/40, .data+.bss+stack {size:#x} ({size/1024:.0f} KiB)")
print(f"    heap {heap:#010x}..{heap+HEAP_LEN:#010x}, "
      f"{(RAM_TOP - heap - HEAP_LEN)/1024:.0f} KiB left for the stack")
if pokes > 32:
    sys.exit(f"    poke table nearly full ({pokes}/40): move a non-zero static to .bss")
if heap + HEAP_LEN >= RAM_TOP:
    sys.exit("    heap would collide with the stack")
PY

echo "==> 3/3 sign (developer key) + uf2"
cargo run --release --package xous-tools --bin xous-sign-image -- \
    --loader-image "${PRESIGN}" \
    --loader-key "${SIGNING_KEY}" \
    --loader-output "${IMG}" \
    --min-xous-ver "${MIN_XOUS_VER}" \
    --sig-length "${SIGBLOCK_LEN}" \
    --with-jump \
    --bao1x \
    --function-code baremetal

echo
echo "artifacts in ${OUTDIR}:"
ls -la "${OUTDIR}" | grep -E 'badgyos' || true

# Block count is the one number worth watching between builds: boot1 writes UF2
# blocks straight into ReRAM, and the window it accepts ends at 0x603D_A000.
if [[ -f "${OUTDIR}/badgyos.uf2" ]]; then
    python3 - "${OUTDIR}/badgyos.uf2" <<'PY'
import struct, sys
d = open(sys.argv[1], 'rb').read()
n = len(d) // 512
lo = struct.unpack_from('<I', d, 12)[0]
hi = struct.unpack_from('<I', d, 12 + 512 * (n - 1))[0] + 256
print(f"    {n} UF2 blocks, {lo:#010x}..{hi:#010x} ({(hi-lo)/1024:.0f} KiB of ReRAM)")
PY
fi
