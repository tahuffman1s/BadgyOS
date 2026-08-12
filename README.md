# BadgyOS

Custom firmware for the **DEF CON 34 badge**. The stock image is gone — no Xous
kernel, no processes, no swap — and in its place: a badger called **Badgy**, a
USB drive you drag Python files onto, and a small interpreter called **pycon**
that runs them on the badge.

[![ci](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)

![Badgy on the home screen](docs/badgy-home.png)

```
   home screen                menu                     scripts
  +------------------+  +------------------+     +------------------+
  |     BadgyOS   1* |  |#    BadgyOS     #|     |#    SCRIPTS     #|
  | 0 F+  ,--. >= V  |  |>Scripts          |     |>blink.py         |
  |  D  ( oo )  - +  |  | Tasks            |     | bounce.py        |
  |   N> `-vv-' <N   |  | USB Drive        |     | hello.py         |
  |  +---(    )---+  |  | Demos            |     | my cool script~  |
  |  E   |uuuu|   %  |  | Button Test      |     | Back             |
  |                  |  | Display          |     |                  |
  |  -PUSH WHEEL-    |  | System Info      |     |                  |
  +------------------+  +------------------+     +------------------+
       128x128 SH1107, 21x10 characters of 6x12 font
```

| | |
|---|---|
| **Target** | DEF CON 34 badge core module (baosec-lite class), Baochip‑1x SoC |
| **Shape** | `no_std` + `alloc`, no interrupts, one loop passed between cooperative tasks |
| **Image** | one signed `badgyos.uf2`, 845 UF2 blocks (211 KiB of ReRAM) |
| **Language** | `pycon` — a Python subset in 4.2 kLOC, zero dependencies |
| **Tested** | 127 host tests over the language and the filesystem |
| **Warning** | flashing it **destroys your badge's `k0` permanently** — [read this](#read-this-before-flashing) |

---

## Badgy

The badge has a mascot, in the Flipper Zero sense: a character who lives on the
home screen and reacts to what the firmware is doing.

![The sprite sheet](docs/badgy-sheet.png)

| Mood | When | What you see |
|---|---|---|
| idle | nothing happening | slow breathing cycle, an occasional blink at irregular intervals |
| digging | a host is writing to the script drive | both paws going, dirt flying |
| plugged in | a host just mounted the drive | Badgy holds up the cable |
| asleep | ~45 seconds without a key press | eyes shut, `z z z` |
| rattled | the last script blew up | wide eyes, brows up, one bead of sweat |

He is a 72×74 **three-state** sprite — lit, dark, transparent — which is what
lets him sit on top of the animated background: his dark pixels black the matrix
rain out, his transparent ones let it through, and no plate has to be cut out of
the screen to make him readable.

The art is drawn from primitives by `tools/badger.py` and committed as
`src/sprites.rs`, one string per row:

```
        "              ###..............#########..............####              ",
        "               ##..............#########..............###               ",
        "                ###............#########.............###                ",
        "                 ####..........########............####                 ",
        "                   ###.........###....#...........###                   ",
```

That file is generated but it is also **source**: `#` is lit, `.` is dark, a
space is transparent, and editing a pixel with a text editor is a perfectly good
way to change the drawing. CI re-runs the generator and diffs, so a hand edit has
to be ported back into `tools/badger.py` rather than silently drifting.

```bash
./tools/badger.py preview    # -> preview/, every frame as a PNG at 6x
./tools/badger.py screen     # -> preview/badgy-home.png, the whole 128x128 panel
./tools/badger.py emit       # -> src/sprites.rs
./tools/badger.py docs       # -> the two PNGs in this README
```

The `screen` command composites the real panel layout using the firmware's own
`font6x12_1bpp.raw`, at the offsets `src/app.rs` uses. That is how the picture at
the top of this README was made — not a mockup of the design, the design itself.

Why a generator: a badger is mostly ellipses, and an ellipse typed by hand at
1bpp looks like a potato. Two things fell out of being able to look at renders
instead of imagining them — the first draft was unmistakably a **panda** (round
head, big ears on top), and the pass that restores the silhouette's rim was
turning every raised arm into a **white lump on the shoulder**, because a 5px
limb eroded by 2 is all rim. Both are one-line fixes and neither is visible in
source.

---

## The script drive

Plug the badge into a computer and a 512 KiB removable volume called **`BADGYOS`**
appears (`1d50:6199`, "BadgyOS" — the *default* identity; see below). Drop `.py`
files on it. About half a second after the copy finishes — or immediately, if you
eject — they appear under **Scripts** in the badge's menu. Delete a file to remove
it.

The volume is formatted on first boot with `readme.txt` and the sample scripts
(`hello.py`, `bounce.py`, `keys.py`, `jiggle.py`, `usbid.py`) on it, so the drive
documents itself.

Scripts survive a power cycle: the whole volume is mirrored into on-chip ReRAM
whenever it goes quiet.

**Hold LEFT + CENTER to stop a running script.** Two keys, because single keys
belong to the script — `keys()` is part of the API.

```
host --USB MSC--> RAM disk (.bss) --FAT12--> scripts --pycon--> OLED
                      |
                   ReRAM @ 0x6020_0000 (survives power loss)
```

---

## pycon

A subset of Python, sized to what is useful on a 128×128 one-bit screen:
integers (32-bit, wrapping), strings, bools, `None` and lists;
`if`/`elif`/`else`, `while`, `for ... in`, `break`, `continue`;
`def`/`return`/`global`; the usual arithmetic, comparison, boolean and bitwise
operators; indexing, and the common list and string methods.

No classes, imports, exceptions, closures, lambdas, dicts, tuples,
comprehensions or floats. Each of those costs more code than it earns here, and
floats in particular would pull in soft-float routines larger than the whole
interpreter. Using one is a clean syntax error, not a silent surprise.

```python
# bounce.py -- nothing appears until show() is called
x, y, dx, dy = 20, 30, 3, 2      # (no tuples, actually: one per line)
while True:
    clear()
    rect(x, y, x + 10, y + 10, True)
    text(2, 2, 'hits ' + str(hits))
    show()
    x = x + dx
    if x <= 0 or x >= WIDTH - 11:
        dx = -dx
    if keys() != 0:
        break
```

| | |
|---|---|
| **draw** | `clear()` `pixel(x,y[,on])` `line(x0,y0,x1,y1)` `rect(x0,y0,x1,y1[,fill])` `text(x,y,s[,on])` `show()` |
| **input** | `keys()` `wait_key()` |
| **mouse** | `mouse_ready()` `mouse_move(dx,dy[,wheel])` `mouse_buttons(mask)` `mouse_click([button])` |
| **usb** | `usb_vid()` `usb_pid()` `usb_id(pid)` / `usb_id(vid,pid)` `usb_name(s)` |
| **other** | `sleep(ms)` `rand([n[,m]])` `print(...)` → serial console |
| **builtins** | `len str int bool chr ord hex abs min max sum range` |
| **list** | `append pop insert remove index count clear reverse extend copy sort` |
| **str** | `upper lower strip startswith endswith find replace split join` |
| **consts** | `WIDTH HEIGHT KEY_* MOUSE_LEFT MOUSE_RIGHT MOUSE_MIDDLE MOUSE_MAX USB_VID USB_PID` |

### The USB device is script-driven, identity and all

The badge enumerates as one composite device with two interfaces: the
mass-storage **script drive** (interface 0) and a boot-protocol **HID mouse**
(interface 1). A script reaches every subsystem the same way — a function call
that does one concrete thing to the hardware and returns whether it took — and
that includes what the badge *calls itself*:

- **Mouse.** `mouse_move`/`mouse_buttons`/`mouse_click` post HID reports;
  `mouse_ready()` says whether a host is listening. Relative motion, clamped to
  `MOUSE_MAX` (127) per report, positive `dy` downward. It cannot wake a
  suspended host (that needs remote wakeup, which this self-powered device does
  not claim) — it keeps an awake one from going idle. See `jiggle.py`.
- **Identity.** Vendor id, product id and product name are runtime state, not
  compile-time constants. `usb_id(...)` / `usb_name(...)` change them and
  **re-enumerate** — the only way a host that already latched an identity will
  notice, so the drive blinks out and comes back on each call. `USB_VID`/`USB_PID`
  are the defaults, so a script can restore them; the one pair `usb_id()` refuses
  is the bootloader's `1d50:6196`, which the flasher matches exactly. See
  `usbid.py`.

Because the identity is applied by re-enumeration rather than baked into the
image, none of the script-loading path changed to support it — a script is still
text on a FAT volume, run through the interpreter, drawing and now also
*presenting* through the `Host` trait.

---

## Controls

The badge's side wheel is a Haoyu TS-1513B: a one-dimensional directional switch
with a center press. Rolling it and pressing it are three separate contacts in
the same 2×3 matrix as the three face buttons.

| Input | Menu | Home screen | Demo | Button test | USB drive | Badgy | Tasks |
|---|---|---|---|---|---|---|---|
| wheel roll | move cursor | open menu | exit | — | — | next frame | move cursor |
| wheel press | select | open menu | exit | hold 1s to exit | hold 2s to format | back | view / result |
| left button | back | open menu | exit | — | back | back | back |
| center | select | open menu | exit | — | back | back | view / result |
| right | select | open menu | exit | — | back | back | stop / clear |

In the **scripts** list, wheel-press and center run a script in front of you;
**right** starts it in the background instead. Either way it is a task — running
one in the foreground only means its page is the one on screen.

While a script is on screen it owns the keys, so the two ways out are chords:

| Chord | What it does |
|---|---|
| **LEFT + CENTER** | stop the script (held ~3 ticks; the script sees it too) |
| **LEFT + RIGHT** | leave it running and go to **Tasks** |

**Tasks** lists what is running, with its state, its share of the CPU, and — for
the row under the cursor — how deep its stack has been and what is left of the
heap. **Right** on a running task stops it, on a finished one clears the row, and
on the **Back** row stops everything. The home screen shows `n*` in the corner
when `n` scripts are running behind it.

---

## What "the stock image is gone" means

| Stock badge image | BadgyOS |
|---|---|
| `loader.uf2` + `xous.uf2` + `swap.uf2` | one `badgyos.uf2` |
| Xous microkernel, MMU, processes, IPC | none — single-threaded bare metal |
| swap, PDDB, off-chip SPI flash | never touched |
| `dc34-vault` (FIDO2/CTAP2, TOTP, passwords) | gone |
| `dc34-console` REPL, LED manager, power manager | gone |
| light-genetics / QR breeding game | gone (and `k0` with it) |
| composite CDC + mass-storage USB | mass storage + HID mouse, polled |
| camera, accelerometer, TRNG, BIO | never initialized |
| interrupts | none — one polling loop |
| preemptive threads, MMU-isolated | 4 cooperative tasks, one address space |
| 690 UF2 blocks (loader alone) | **845 UF2 blocks** |

What is left: reset vector, clock/PLL bring-up, a timer, a transmit-only serial
console, the SH1107 driver, a polled key matrix, an 864-byte bitmap font, a USB
device stack, a FAT12 reader, a Python interpreter, and a badger.

---

## Layout

```
bootstrap.sh    put a pinned xous-core checkout at vendor/xous-core
build.sh        build -> copy-object -> sign -> .uf2, and print the memory map
xous-core.pin   the upstream commit this is built against
link.x          FLASH ORIGIN = 0x6006_0400
rustfmt.toml    xous-core's style, so diffs against that tree are about behaviour

src/
  asm.rs        _start: set sp, install trap handler, jump to rust_entry
  platform.rs   early_init: SRAM trim, statics, clocks, heap, timer, display power
  debug.rs      TX-only console, UART2 @ PB14, 1 Mbaud 8N1
  gfx.rs        6x12 text (vendored from boot1) + lines, rects, sprites
  sprites.rs    Badgy, 8 frames of 72x74, generated by tools/badger.py
  badgy.rs      which frame to draw: moods, blink timing, captions
  input.rs      jog wheel + buttons: 2x3 matrix scan, debounce, auto-repeat
  anim.rs       ASCII animations: matrix rain, doom fire, plasma
  menu.rs       scrolling list widget, the static menu tree, the ItemList seam
  sched.rs      green threads: the context switch, task table, pages, kill
  app.rs        screen state machine, the polling loop, and the compositor
  usb/
    mod.rs      attach / poll / reattach, the SE0 dance, pointer window checks
    proto.rs    descriptors and EP0 control transfers
    msc.rs      Bulk-Only Transport + SCSI over a 512 KiB RAM disk
    hid.rs      boot-protocol mouse reports
  store.rs      mirror the volume into ReRAM at 0x6020_0000
  scripts.rs    idle detection, rescan, import, and the seeded sample files
  runner.rs     the Host that pycon draws and reads keys through
  util.rs       no-alloc string formatting, xorshift PRNG, integer hash
  main.rs       bring-up, then hand off to app

pycon/          the language: lexer, parser, arena AST, tree-walking evaluator
badgy-fat/      FAT12/16 reader + formatter
samples/        the scripts seeded onto a fresh volume
tools/
  badger.py     draw Badgy; render previews; emit src/sprites.rs
  badgeflash.py wait for the badge, validate the UF2, push it
```

### The two crates that are not firmware

`pycon` and `badgy-fat` have no hardware dependencies at all — one talks to a
`Host` trait, the other to a `&[u8]`. That is on purpose: they hold most of the
logic and all of the parsing of untrusted input, and they are the only parts of
this firmware that can be tested without a badge.

```bash
cargo test --target x86_64-unknown-linux-gnu -p pycon -p badgy-fat --features std
```

127 tests, and the FAT ones are worth a look: they shell out to `mkfs.fat` and
`fsck.fat` and check the parser against volumes real tools produced, and pin the
long-filename path against directory entries captured byte-for-byte from a Linux
host doing a `cp`. Reading FAT from the spec is how you write a parser that only
works on your own output.

Keep it that way. Anything that touches a register goes behind a trait.

---

## Build

```bash
./bootstrap.sh      # once: put a xous-core checkout at vendor/xous-core
./build.sh          # -> target/riscv32imac-unknown-none-elf/release/badgyos.uf2
```

You need the `riscv32imac-unknown-none-elf` target (`rustup target add
riscv32imac-unknown-none-elf`) — *not* the Xous target, since there is no Xous
here.

### Why there is a bootstrap step

BadgyOS links the Baochip board-support crates (`bao1x-hal`, `bao1x-api`,
`ux-api`, `utralib`, `svd2utra`) straight out of the Xous tree, and calls its
host tools (`xous-copy-object`, `xous-sign-image`) to package and sign the image.
None of that is published to crates.io in a form carrying the bao1x register
definitions, so the dependency is a **pinned checkout** rather than a version
number. `xous-core.pin` names the commit; `bootstrap.sh` gets one, in this order:

1. `$XOUS_CORE` points at a checkout → symlink it.
2. A sibling clone is lying around (`../baoom/upstream/xous-core`, `../xous-core`,
   …) → symlink it.
3. Neither → shallow, blobless clone of the pinned commit.

The symlink cases exist because a xous-core working tree is a couple of
gigabytes; if you already have one there is no reason for a second. The clone
case is what CI uses. `vendor/` is gitignored either way — it is upstream's code,
used read-only, and nothing here ever writes to it.

`build.sh` prints the two limits that are easy to blow past without noticing:

```
    statics v1: pokes 3/40, .data+.bss+stack 0x86000 (536 KiB)
    heap 0x61086000..0x61146000, 744 KiB left for the stack
    820 UF2 blocks, 0x60060000..0x60093400 (205 KiB of ReRAM)
```

The poke table holds one entry per non-zero word of `.data`, capped at 40, and it
fails at *boot* rather than at build time — which is why the 512 KiB RAM disk is
a zero-initialized `static mut` in `.bss` (NOBITS: no image bytes, no pokes)
rather than anything with an initializer. It is also why Badgy costs nothing but
image bytes: `link.x` maps `.rodata` into FLASH, so 42 KiB of sprite strings add
zero pokes.

To add a screen: give it an `Action` in `src/menu.rs`, list it in a `MenuDef`,
and handle that action plus a `Screen` variant in `src/app.rs`.

### CI

`.github/workflows/ci.yml` runs three jobs:

- **host** — the 127 tests, a no-`std` build of both library crates, `rustfmt
  --check` and `clippy -D warnings`. Needs no badge, no riscv target and no
  xous-core. This is what gates a merge.
- **sprites** — re-runs `tools/badger.py emit` and diffs `src/sprites.rs`.
- **firmware** — bootstraps the pinned xous-core (cached on the pin), builds,
  signs, validates the image the way boot1 will, and uploads `badgyos.uf2`.

It cannot check that the firmware *works*. That takes a badge and a pair of eyes.

Formatting needs **nightly** rustfmt (`rustfmt.toml` uses unstable options, to
match xous-core's layout); everything else builds on stable:

```bash
cargo +nightly fmt -p badgyos -p pycon -p badgy-fat
```

Note the explicit `-p` list rather than `--all`: `cargo fmt --all` follows the
path dependencies into `vendor/xous-core` and tries to reformat code we do not
own — and panics on a `macro_rules!` in its locales crate on the way.

Tagging `v*` runs `.github/workflows/release.yml`, which builds from the pin and
attaches the `.uf2` plus a checksum to a GitHub release.

---

## Where it lands

`BAREMETAL_START == LOADER_START == 0x6006_0000`. There is exactly one slot, so
this image **displaces the Xous loader**. boot1 does not distinguish the two: it
makes a single `validate_image(BOOT1_TO_LOADER_OR_BAREMETAL, ...)` call that
accepts function codes `Baremetal`, `UpdatedBaremetal`, `Loader` and
`UpdatedLoader` interchangeably, and it requires no kernel or swap to be present
(`bao1x-boot/boot1/src/secboot.rs:61`, `libs/bao1x-api/src/pubkeys/mod.rs:89`).

boot1 tries `LOADER_START` *before* it initializes its own OLED, USB or SPI
flash, so this firmware starts almost immediately after boot1's security checks.

---

## Read this before flashing

### 1. It is signed with the developer key, and that is irreversible

`build.sh` signs with `xous-core/devkey/dev.key`, whose private half is public —
that is the only key we have. When boot1 validates a developer-signed image it
calls `erase_secrets()` and increments the `DEVELOPER_MODE` one-way counter
(OWC slot 85, `libs/bao1x-hal/src/sigcheck.rs:761-769`). Consequences:

- **`k0`, the DEF CON light-encryption key, is destroyed and cannot be
  recovered.** The PDDB ciphertext in off-chip flash survives, but its master key
  is HKDF'd from `ROOT_SEED` + nuisance/chaff keys (all erased) *and* the HKDF
  `info` string flips from `b"sec"` to `b"dev"`. Reflashing stock does not bring
  it back. The badge is out of the conference light-breeding game permanently.
- The erase re-runs on **every** subsequent boot once the bit is set
  (`secboot.rs:31-39`). There is no way back.
- Erased slots on baosec: `THE_FLAG_1`, `CP_COOKIE`, `RMA_KEY`, `ROOT_SEED`,
  `NUISANCE_KEYS_0/1`, `CHAFF_KEYS`. `SWAP_KEY` is deliberately spared.

Stock badge images are signed with the **beta** key (slot 2), not the developer
key — so a factory badge has not entered developer mode, and flashing this is
what pushes it over.

Because the signing key is public, a signature on one of these images says
nothing about who built it. Build your own, or check the checksum.

### 2. Check that the developer key is not revoked — before you flash

`lockdown` at the boot1 console revokes the developer key. `README-baochip.md:93`
says "`baosec` boards have this done at the factory", which would make this image
unbootable. Reading the tree, that looks like it refers to the baosec USB-token
SKU rather than the badge core module: `lockdown` is a manual two-phase console
command (`bao1x-boot/boot1/src/repl.rs:698`) with no automated factory call site
anywhere in xous-core, and `dc34-vault/README.md` states plainly that loading
your own firmware puts the badge into developer mode — which is impossible if the
key were revoked.

That is an inference from source, not a fact about how your unit was provisioned.
Confirm it non-destructively first:

1. Hold a button while plugging in, to enter boot1.
2. Open its console. boot1 enumerates as **one composite device** (`1d50:6196`)
   carrying both a CDC serial port and the mass-storage volume, so the REPL is on
   `/dev/ttyACM0` on the same cable you flash over — no probe on PB14/PB13
   needed. Baud is irrelevant.
   ⚠️ Some CDC stacks reset the device when DTR is asserted, and a reset in
   update mode **commits the pending image**. Open the port with DTR/RTS left
   alone if you are inspecting before deciding to commit.
3. Run `audit` and read the revocation matrix (`bao1x-boot/boot1/src/audit.rs:100`):

```
Revocations:
Stage       key0     key1     key2     key3
boot0       ...
boot1       ...                       <-- key3 is the developer key
next stage  ...
```

The **`boot1` row, `key3`** column is the one that gates this image
(`BOOT1_TO_LOADER_OR_BAREMETAL` uses `BOOT1_REVOCATION_OFFSET`, not
`LOADER_REVOCATION_OFFSET`). If it reads `enabled`, this firmware will boot. If
it reads `revoked`, it will not, and nothing you do here can change that.

**Writing the UF2 is not the destructive step.** Dropping a file on `BAOCHIP`
only fills ReRAM; that path never validates anything. `validate_image` is reached
from exactly two places: the boot path (`secboot.rs:63,113`, which passes a
csprng and so *can* erase secrets) and `audit.rs:155` (which passes `None` — that
is *why* `audit` cannot erase anything, not merely convention). So the erase and
the counter bump happen on the **next boot**. Until you press a button, the badge
is intact and its console is live. That is the last window to run `audit`.

### 3. Restoring stock is not blocked

Anti-rollback counters are **per function code**: baremetal uses OWC slot 64,
loader slot 60, kernel 61, swap 62. Booting this image can only advance slot 64,
so it can never block re-flashing the stock loader/xous/swap. All in-tree
anti-rollback values are `1`, and all three shipped 2026-07-30 stock images carry
`anti_rollback == 1`, so restoring passes the check by equality.

To restore: copy the stock `{loader,xous,swap}.uf2` back onto the `BAOCHIP`
volume. The three files cover their regions completely. boot1's UF2 writer never
erases a partition, but `validate_image` hashes exactly `signed_len` bytes, so
the smaller BadgyOS image leaves no harmful residue.

The badge boots stock again — minus `k0`, and flagged as a developer device
forever.

---

## Flashing

`tools/badgeflash.py` waits for the badge, validates the image and pushes it:

```bash
./tools/badgeflash.py                 # flashes this repo's build output
./tools/badgeflash.py --dry-run       # just validate, touch no hardware
```

```
:: validating 1 image(s)
  badgyos.uf2
    820 blocks, 209920 bytes payload -> 0x60060000..0x60093400  [ReRAM (on-chip)]
    signed: Baremetal (code 6), anti-rollback 1, ed25519 only, covers image (+224B pad)
    version: v0.10.2-0 (min v0.9.8-791)
ok dry run: images look flashable
```

Then hold any button while pressing reset / plugging in USB. Press any button
again once it reports success, to commit and boot.

Manually, if you prefer:

1. Hold any button while pressing reset / plugging in USB → update mode, the
   device enumerates as a mass-storage volume labelled `BAOCHIP`.
2. Copy `badgyos.uf2` onto it.
3. `sync` (or unmount) so the write completes.
4. Press any button to commit and boot.

Validating first is worth the extra second: boot1's UF2 receiver silently drops
any block whose family ID (`0xa7d7_6373`) or target address it dislikes
(`bao1x-boot/boot1/src/platform/bao1x/usb/handlers.rs:249`). A bad image is not
reported as an error — you just get a half-written partition that fails its
signature check on the next boot. Valid windows are ReRAM
`0x6006_0000..0x603D_A000` and swap `0x7000_0000..0x7800_0000`.

You should see Badgy on the OLED and this on the serial console:

```
BadgyOS console up
=== BadgyOS ===
CPU 350 MHz / perclk 99 MHz
wheel + buttons ready; entering UI.
```

Menu selections are echoed to the console as you make them, and the button test
screen echoes every key press — so the matrix can be checked from a terminal
with the badge face-down on a bench.

### What has actually been run on hardware

Verified on a real badge (serial `M36DF1`) at the `dc34-minimal` revision this
repo grew out of: enumeration as `1d50:6199`, polled USB with no interrupt and no
trap handler, the FAT12 superfloppy mounting on Linux with no MBR, the formatter
and seed files round-tripping, and host→badge writes landing byte-for-byte across
an unmount/remount cycle (see the TRB-chaining note below — that one was found
the hard way).

**Not yet verified on hardware:** Badgy himself, running a script from the menu,
and ReRAM persistence across a power cycle. Windows and macOS have not been tried
at all — keep `VOL_LBA_BASE` parameterized in case one of them needs the MBR
wrapper.

**Also not yet verified on hardware: the scheduler.** It builds, it packages, and
the 127 host tests still pass — but none of them exercise a context switch,
because the switch is assembly for a target the test bench does not run. What to
watch for on a badge, in the order they would show up: whether the first switch
into a fresh task lands in the trampoline at all (a wrong `ra` offset or a stack
that is not 16-byte aligned would trap straight to `abort`, i.e. a screen full of
`u` on the DUART); whether `Tasks` shows a plausible stack high-water mark
(48 KiB was reasoned about, not measured — the manager exists to replace the
reasoning with a number); and whether a backgrounded animation keeps the same
speed it had in front, which is the `FRAME_MS` pacing doing its job.

---

## Running more than one script

Up to three scripts run at once, each on its own stack, switched by hand. The
whole kernel is `src/sched.rs` — a task table, twenty-five instructions of
assembly, and the rule about who gets the screen.

```
        the loop, handed around
   +----------+   yield   +----------+   yield   +----------+
   | task 0   | --------> | blink.py | --------> | jiggle.py|
   | the UI   | <-------------------------------------------+
   +----------+
   draws to the panel        each draws into its own 2 KiB page
```

**Why cooperative and not preemptive.** Cooperative scheduling normally has one
fatal flaw — a task that never yields wedges the machine, and `while True: pass`
is a legal pycon program. That flaw does not exist here, because the interpreter
already yields on a cadence *it* controls rather than one the script controls:
`Interp::steps` calls `Host::tick` every 512 statements whatever those statements
are, and every blocking builtin (`sleep`, `wait_key`, `show`) funnels through the
same place. A pycon script cannot fail to reach a scheduling point. Preemption
would buy only the ability to reap a task wedged in *Rust*, and would cost
putting the allocator's spin lock, `usb::poll()` and the ReRAM commit sequence
within reach of reentrancy — the same trade this firmware already refused when it
chose to poll the USB controller instead of taking its interrupt.

**The switch.** Push `ra` and `s0`–`s11`, store `sp` in the outgoing task, load
`sp` from the incoming one, pop, return. Everything else is caller-saved and
there is no float state on rv32imac. A new task's stack is hand-built with a
frame whose return address is the trampoline, so the first switch into it
"returns" into a task that has never run.

**Nobody draws to the panel.** There is one OLED and there can be three scripts,
so each task draws into its own 128×128 page and the UI task copies whichever one
has focus onto glass. That is what makes the task manager a screen-switcher, and
it means `show()` no longer blocks on 14 ms of SPI — so it is *made* to cost 14 ms
anyway, spent on other tasks, or every backgrounded animation would silently run
an order of magnitude fast.

**Stopping one is not a `kill(2).`** Nothing tears a task's stack down from
outside. `sched::kill` sets a flag that the interpreter's existing abort path
reads, so the script unwinds exactly as it does for the exit chord: values freed,
`Drop` run, and any mouse button it was holding released. Reaping a stack instead
would leak every one of those. The cost is that a task wedged below the
interpreter — which pycon cannot arrange, but a bug in a builtin could — is not
killable at all, and that is the honest price of the design.

**The bug this design had to be built around:** timer0 is a 1 ms auto-reload with
a *sticky, one-bit* event flag. Acknowledging it throws away however many
milliseconds actually passed. With one loop that was harmless — whoever spun on
the flag was also the only thing that cared. With three tasks it silently halves
everyone's clock and every `sleep()` runs long. So the flag now has exactly one
reader (`platform::tick_clock`) and everything else asks it for the time.

Other things that had to be shared out, and how:

| | |
|---|---|
| the keys | only the focused task sees them; the rest read an empty matrix |
| the mouse and USB identity | one bus, so first task to ask keeps them; others get `False`, which the API could already say |
| the heap | not partitioned. 768 KiB between three scripts, and whoever crosses `HEAP_RESERVE` is the one that fails |
| the stacks | 48 KiB each in `.bss`, with a poisoned kilobyte at the bottom probed on every switch — under three tasks, running off the end would corrupt a *neighbour*, so it stops the task with "ran out of stack" instead |
| the allocator | its lock is never held across a switch, because switches happen only where `sched` puts them and none is inside `alloc`. True of cooperative scheduling, false of preemption |

What it costs: 96 KiB more `.bss` (688 KiB total, still 592 KiB clear under the
boot stack), 25 UF2 blocks, and about thirty instructions per switch. What is
unchanged: `pycon` itself, which needed no modification at all — the `Host` trait
was already the right seam — so all 127 host tests still run on a laptop.

---

## Notes on the implementation

- **`oem-baosec-lite` is not cosmetic.** The DC34 badge is a "lite" baosec, which
  moves the peripheral reset line from **PC6 (active low)** to **PA6 (active
  high)** (`libs/bao1x-hal/src/board/baosec.rs:258-308`). `Oled128x128::new()`
  drives that line, so building without the feature configures the wrong pin and
  the panel stays dark. It is on by default here; the `baosec-lite` cargo feature
  turns it off for a full baosec devkit. Everything else about the display —
  panel, SPIM channel 2, pins PC0/PC1/PC2/PC3, power pin PC4 — is identical
  between the two.

- **The OLED power rail is driven explicitly.** `setup_oled_power_pin()` only
  *configures* PC4; it never drives it. On a stock badge that write happens once,
  in boot1's `early_init`, and the Xous loader simply inherits the state.
  `platform::setup_display_power()` does it itself so this firmware does not
  depend on which path boot1 took.

- **Text without a graphics server.** `ux-api`'s normal text path (blitstr2)
  wants `std`. The renderer in `src/gfx.rs` is instead vendored from
  `bao1x-boot/boot1/src/platform/bao1x/gfx.rs`: a 1bpp 6x12 font atlas walked
  with `put_pixel()`. That is how boot1 draws "Update mode" / "Booting...", so it
  is a proven `no_std` path. Pixel polarity is inverted from intuition — a set
  bit is dark, `Mono::White → ColorNative(0)` — which is also why Badgy's *lit*
  pixels are the white parts of a badger.

- **The upstream key-matrix decode has row 1 wrong**, which is why `src/input.rs`
  does its own scan. `bao1x_hal::board::scan_keyboard` maps `(1, 0) => Right`,
  `(1, 2) => Center` and `(1, 3) => Left` — but there are only three columns, so
  `Left` is unreachable and `(1, 1)` falls through to `Invalid`. The keypad
  controller's own decode in the same file (`kpc_sr0_to_key`, bit positions 4/5/6)
  and `dc34-core-hw`'s netlist both say SW5/SW3/SW4 are Left/Right/Center in
  columns 0/1/2. Row 0 — the wheel — is mapped correctly in both. Upstream's
  version also `println!`s every press and re-runs `setup_kb_pins` per call,
  neither of which suits a poll loop.

- **The wheel is three switches, not an encoder.** SW2's CCW, PUSH and CW
  contacts sit on `(row 0, col 0/1/2)`, so a detent is one momentary closure and
  ordinary debouncing is enough — there is no quadrature to decode. SW4 is the
  odd one out: it drives a FET gate rather than shorting its row to its column,
  but it still reads at `(row 1, col 2)`.

- **UI timing is counted in polls and frames, not milliseconds.** A full `draw()`
  pushes 2 KiB over a 2 MHz SPI and costs roughly three polls, and timer0 is a
  1 ms auto-reload with a *sticky, one-bit* event flag — so a millisecond clock
  polled from the render loop silently loses every tick spent drawing. Debounce,
  key repeat, the animation cadence, the hold-to-exit and Badgy's blink are all
  in loop iterations or animation frames instead, which is a unit the loop can
  actually measure. The scheduler does keep a millisecond count, because `sleep()`
  and the CPU meter need one — it has the same blind spot, and says so.

- **The heap's start is derived, not hardcoded.** It is 256 KiB placed
  immediately above the region `early_init` zeroizes, read from the statics table
  at runtime. xous-core's `baremetal` instead pins `HEAP_START = RAM_BASE +
  0x6000`, which works only by coincidence: `link.x` sizes `.stack` as
  `. += 16K; . = ALIGN(4096)`, so `_sheap` jumps a page as soon as `.data`
  crosses a 4 KiB boundary, and the zeroize loop would then clear the first page
  of the heap. Deriving it removes the cliff — and with the RAM disk in `.bss`
  the two values now disagree by half a megabyte.

- **USB is polled, not interrupt-driven, and that is a choice.** boot1 says
  outright that "USB is entirely interrupt driven, so there is no loop to handle
  it", and xous-core's `baremetal` agrees. But their interrupt handler
  (`baremetal/src/platform/bao1x/irq.rs:202-250`) is a hand-inlined copy of
  `CorigineUsb::udc_handle_interrupt()`, which reads `USBSTS`, drains the event
  ring by walking a cycle bit, and re-arms. Nothing in it needs to have arrived
  via a trap. So `usb::poll()` calls that same function from the main loop, and
  the firmware keeps its no-interrupt shape — which avoids porting a 258-line
  register-save trampoline, and avoids putting the allocator's spin lock and the
  ReRAM commit sequence (which the HAL documents as unsafe under concurrency)
  within reach of reentrancy. **Confirmed on hardware:** `mie.MEXT` is never set
  and the drive works.

  The cost is latency. The longest the loop goes without polling is one
  `Oled128x128::draw()`, about 14 ms, which is inside the 50 ms a host allows for
  `SET_ADDRESS` and irrelevant to bulk traffic — an unserviced bulk transfer is
  NAKed and retried, which is ordinary USB. Every wait in the firmware runs
  through `sched::yield_now()`, which polls the controller on the way past, so
  that 14 ms really is the worst case no matter which task is busy.

- **The drive is a real filesystem, not boot1's trick.** boot1 never stores what
  the host writes: it sniffs the sector stream for UF2 magic and discards the
  rest, because a firmware image is self-describing and it never needs to know a
  filename. A `.py` file is not self-describing, so here every written sector
  lands in the RAM disk and is served back on the next read. That also keeps the
  host's cache and ours in agreement, which is what stops an OS from deciding the
  volume is damaged and "repairing" it into something neither side understands.

- **A host does not tell you when it has finished copying.** Watching a real `cp`
  onto a FAT12 volume: the directory entry appears *first* with size 0, then again
  with an intermediate size and a plausible cluster chain, and only then with the
  real size — and the data sectors arrive out of order. Two independent guards
  keep a half-written file out of the menu. The scan only runs once write traffic
  has stopped for 128 loop passes (or immediately on `SYNCHRONIZE CACHE` or an
  eject), and `Volume::files()` refuses any file whose cluster chain does not have
  exactly as many links as its recorded size needs and terminate properly. It is
  also the signal Badgy digs on.

- **Long filenames are not optional.** Linux writes a VFAT long-name entry even
  for a plain `blink.py`, because the 8.3 name it stores alongside is uppercased
  to `BLINK.PY`. Reading only the short entry would show the user a name they did
  not choose. `badgy-fat` assembles the long name, verifies its checksum against
  the short entry, and falls back if they disagree.

- **FAT12 is arithmetic, not preference.** The type is decided by cluster count —
  under 4085 clusters is FAT12 by definition — and 4085 clusters of 512 bytes
  needs a volume over 2 MiB. At 512 KiB there is no other legal option, whatever
  the `"FAT12   "` string in the boot sector claims. (boot1 gets to use FAT32
  because it wanted a 128 MiB volume for firmware images.) Geometry that works:
  512 B sectors, 1 sector/cluster, 1 reserved, 2 FATs of 3 sectors, 64 root
  entries, 1024 total sectors → 1013 clusters.

- **Scripts live in ReRAM, not the SPI flash.** The badge has both, and ReRAM wins
  on three counts: it is bit-alterable so there is no erase cycle to coordinate,
  it is memory-mapped so checking what is already stored is a `memcmp`, and it
  needs no peripheral bring-up where the SPI part would need pins, a UDMA channel,
  a QPI negotiation and two pages of IFRAM. The store is
  `0x6020_0000..0x6028_1000` — above this image, far below the one-way counter
  page at `0x603D_A000` where an ordinary store of zero *is* a counter increment,
  and outside the region the boot signature covers. `store::save()` compares
  before writing, so an idle mount costs nothing and a file drop costs a few
  4 KiB pages. The header carrying the checksum is written last, so a power loss
  mid-save fails verification and falls back to a fresh format rather than
  restoring half a volume. Note that `Reram::write_slice`'s own bound is
  `BOOT1_START..RRAM_STORAGE_LEN` — it will happily overwrite boot1 — so it is
  wrapped in a tighter check here.

- **A script is untrusted input that arrived over USB**, on a core with no MMU, no
  guard page, and a heap growing toward a descending stack. Three failure modes
  matter, and they all end the same way — a panic prints and spins until someone
  pulls the power:

  *Time.* Every loop in the evaluator calls `Host::tick` on its back edge, which
  is what makes `while True: pass` interruptible with LEFT+CENTER. Operations
  that could burn a lot of work without reaching a loop are bounded too: `[] *
  2147483647` multiplies out to zero elements but would still spin two billion
  times, so the *count* is checked as well as the product, and `print()` on a
  self-referential list carries a visit budget because the cost there is
  exponential in fan-out, not depth.

  *Memory.* `MAX_LIST_LEN` and `MAX_STR_LEN` cap the individual growth paths, and
  `Host::heap_pressure` catches the general case they cannot — a script that
  simply keeps many small things alive. The firmware answers it from the
  allocator's free count, and the interpreter turns a `true` into an ordinary
  script error while there is still heap left to report it with. Parsing costs
  about 25 KB of heap per KiB of source (the token vector and the AST arena are
  both live at once), which is what sets `MAX_SCRIPT_BYTES = 16 KiB` against a
  768 KiB heap. The lexer `try_reserve_exact`s one token per byte up front so
  `Vec::push` can never panic on a failed allocation.

  *Stack.* `MAX_PARSE_DEPTH`, `MAX_CALL_DEPTH` and `MAX_EVAL_DEPTH` bound
  recursion on the way in. On the way out, `Interp::teardown` empties the value
  graph through an explicit worklist, because `a = [a]` in a loop builds a list
  whose ordinary recursive drop is one frame per level.

  Drawing is clamped rather than trusted: `put_pixel` clips, but `fill_rect`
  iterates the range it is given, so `rect(0, 0, 2000000000, 2000000000)` is
  clamped to the panel before it gets there.

- **Three bugs in `bao1x-hal` that this routes around.** `vendor/` is never
  modified; each of these is commented at the call site.

  ⚠️ **`CorigineUsb::ep_halt` can hang forever on a bulk endpoint.** After
  issuing `SetHalt` it spins on `while self.csr.rf(EPRUNNING_RUNNING) != 0`. But
  `EPRUNNING_RUNNING` is `Field::new(30, 2, EPRUNNING)` — 30 bits covering *every*
  endpoint from PEI 2 up, not the one being halted. The Baochip book documents the
  bit as "asserted if EP is enabled and not Halted or Stopped", and Stopped is
  only reachable via an explicit Stop EP command, so draining the ring does not
  clear it. Halting one of a bulk *pair* therefore spins on its partner, and the
  first call hangs. `ep_disable` in the same file masks with `1 << pei` and is
  correct, which shows the field-wide wait is an oversight. boot1 has the
  identical call pair and survives only because a compliant host never sends a
  malformed CBW. **BadgyOS never halts a bulk endpoint**: it re-arms the command
  receive and lets the host time out and reset. It also acknowledges
  `CLEAR_FEATURE(ENDPOINT_HALT)` rather than stalling it (boot1 stalls, which
  would leave a host unable to finish its own reset).

  ⚠️ **`setup_big_read` has an off-by-one at the end of the disk.** `if (offset +
  actual_len) < disk.len()` copies real data, `else` zero-fills — so a read ending
  *exactly* at `disk.len()`, which is what a host does when it probes the last
  sector to confirm the reported capacity, comes back as zeros. boot1 never
  notices because its disk is a small window inside a 128 MiB advertised volume
  where the tail is synthetic anyway. The fix for a fully-backed disk is to hand
  the HAL a slice one sector longer than anything addressable: `RAMDISK` is
  `DISK_BYTES + SECTOR_SIZE`, with `disk()` and `backing()` for the two uses.

  ⚠️ **A bulk completion event describes one TRB, not one transfer.** Found on
  hardware, after a host `cp` produced a script that failed to parse mid-file.
  `MAX_TRB_XFER_LEN` is 1024, so `bulk_xfer` cuts anything longer into a chain of
  1024-byte TRBs and sets interrupt-on-complete on the **last one only**. The
  event hands the completion handler *that* TRB's length — never more than 1024,
  and exactly 1024 whenever the transfer is a multiple of it. Treating it as the
  length of the whole transfer drops everything past the first kilobyte: a
  4096-byte `cp` stored bytes 0..1023 and left the remaining three clusters at
  their previous contents, while the same file written with `dd bs=1024
  oflag=direct` landed whole. boot1 is not affected, and not by luck: its data
  phase ignores that field and iterates `app_buf[..len]` with the promised length.
  BadgyOS had *added* a `.min(length)` clamp to stop a short-writing host from
  having the shortfall filled in with stale staging bytes — sound hardening, and
  exactly what walked into this. The fix keeps the hardening and gets the length
  right: the completing TRB's data pointer says how much came before it, so
  `(buf_addr - APP_BUF_ADDR) + (programmed - residual)` is the chain total, and a
  short transfer still under-counts rather than over-counts.

- **PF5 is the SE0 switch and boot1 leaves it LOW.** Both boot paths arrive with
  the port held in SE0 and the UDC un-initialized — boot1 drives it low in its own
  `early_init`, and the update-mode path additionally calls `glue::shutdown()`
  before jumping. So a new stage must *re-enumerate*, never adopt: SE0 low →
  `CorigineUsb::new/reset/init/start` → delay → SE0 high. If you ever re-init a
  *running* controller, `stop()` it first or `init()` hangs.

- **Do not reuse VID/PID `1d50:6196`.** That is boot1's bootloader drive, and
  `tools/badgeflash.py` finds the flashable volume by matching exactly that pair —
  a second device answering to it could have UF2s pushed at it. `6197` (dabao) and
  `6198` (baosec Xous) are also taken; BadgyOS uses **`6199`**, and `usb_id()`
  refuses to let a script take `6196`.

- **IFRAM budget for USB + display + flash all live at once**: `CRG_UDC_MEMBASE`
  is IFRAM0 top − 13 pages, 5 pages long (`0x5001_3000..0x5001_6700`);
  `DISPLAY_IFRAM_ADDR` is top − 5 pages; `SPIM_FLASH_IFRAM_ADDR` top − 7;
  `UART_DMA_TX_BUF_PHYS` top − 1. No conflict. IFRAM1 is entirely free on this
  board (it belongs to the camera), so the 8 KiB bulk staging buffer lives at
  `HW_IFRAM1_MEM`, as boot1's does.

- **`bao1x-hal/security` is not optional.** `clocks.rs` and `rram.rs` reference
  `crate::hardening` / `crate::sigcheck` unconditionally, so the crate does not
  compile without the feature even though this firmware verifies nothing.

- **`perclk` is 99_804_688 Hz**, not 100 MHz — `init_clock_asic` divides 350 MHz
  by 256/72. The SPI clock divider works out the same either way.

---

## Reference

- `xous-core/baremetal/` — the in-tree equivalent this is derived from.
  `cargo xtask bao1x-baremetal-baosec` builds it. That target hardcodes the crate
  name `baremetal`, which is why `build.sh` reimplements the pipeline instead of
  calling xtask.
- `xous-core/bao1x-boot/boot1/` — what jumps into this image.
- `xous-core/loader/src/platform/bao1x/bao1x.rs` — the stock occupant of this same
  flash slot; a useful diff when something does not come up.
- Baochip book: <https://baochip.github.io/baochip-1x/> · register docs:
  <https://ci.betrusted.io/bao1x/> and <https://ci.betrusted.io/bao1x-cpu/>
- Badge firmware releases: <https://ci.betrusted.io/releases/latest/baochip/dc34-badge/>

## License

Apache-2.0 (`LICENSE`), matching xous-core, because parts of this are derived
from it: `src/gfx.rs` and `src/font6x12_1bpp.raw` are vendored from boot1 (which
took the font from the `embedded-graphics` crate), and `src/platform.rs`,
`link.x` and `build.rs` are adapted from its `baremetal` target. Badgy, `pycon`
and `badgy-fat` are original work.
