#!/usr/bin/env python3
"""Draw Badgy, and emit him as 1bpp sprite frames for src/sprites.rs.

Why a generator rather than hand-typed art: a badger is mostly ellipses, and an
ellipse typed by hand at 1bpp looks like a potato. Everything here is drawn from
primitives, rendered to PNG so it can actually be *looked at*, and only then
written out as the '#'/'.'/' ' rows the firmware blits.

    ./tools/badger.py preview      # -> preview/badgy-sheet.png, one frame each
    ./tools/badger.py emit         # -> src/sprites.rs
    ./tools/badger.py pycon        # -> the mouse, as rows for samples/jiggle.py

src/sprites.rs is the source of truth for the firmware -- it is committed, and
hand-editing a pixel there is fine and expected. Re-running `emit` overwrites
those edits, so if you tweak by hand, tweak here too or stop using the emitter.

Three pixel states, because Badgy sits on top of an animated background:
    '#'  ink        a lit pixel (white on the OLED)
    '.'  dark       explicitly black -- occludes whatever is behind
    ' '  clear      transparent, outside the silhouette

The badger's own colouring does most of the work: a badger is a white face with
two black stripes through the eyes, over a dark body. On a black OLED with white
ink that maps directly -- white where the badger is white, black where black.
"""

import sys
import zlib
import struct
from contextlib import contextmanager
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

W, H = 72, 74

INK, DARK, CLEAR = 1, 2, 0
CHARS = {INK: "#", DARK: ".", CLEAR: " "}


# --------------------------------------------------------------------- raster


class Canvas:
    def __init__(self, w=W, h=H):
        self.w, self.h = w, h
        self.px = [[CLEAR] * w for _ in range(h)]
        self._clip = None

    def set(self, x, y, v):
        if not (0 <= x < self.w and 0 <= y < self.h):
            return
        if self._clip == "ink" and self.px[y][x] != INK:
            return
        if self._clip == "solid" and self.px[y][x] == CLEAR:
            return
        self.px[y][x] = v

    def get(self, x, y):
        if 0 <= x < self.w and 0 <= y < self.h:
            return self.px[y][x]
        return CLEAR

    def ellipse(self, cx, cy, rx, ry, v=INK):
        for y in range(int(cy - ry) - 1, int(cy + ry) + 2):
            for x in range(int(cx - rx) - 1, int(cx + rx) + 2):
                # +0.5 samples pixel centres, which keeps small circles round
                # instead of diamond-ish.
                dx = (x + 0.5 - cx) / rx
                dy = (y + 0.5 - cy) / ry
                if dx * dx + dy * dy <= 1.0:
                    self.set(x, y, v)

    def poly(self, pts, v=INK):
        """Even-odd scanline fill of a closed polygon."""
        ys = [p[1] for p in pts]
        for y in range(int(min(ys)), int(max(ys)) + 1):
            xs = []
            for i in range(len(pts)):
                (x0, y0), (x1, y1) = pts[i], pts[(i + 1) % len(pts)]
                if y0 == y1:
                    continue
                lo, hi = min(y0, y1), max(y0, y1)
                if lo <= y + 0.5 < hi:
                    t = (y + 0.5 - y0) / (y1 - y0)
                    xs.append(x0 + t * (x1 - x0))
            xs.sort()
            for i in range(0, len(xs) - 1, 2):
                for x in range(int(round(xs[i])), int(round(xs[i + 1])) + 1):
                    self.set(x, y, v)

    def line(self, x0, y0, x1, y1, v=INK, width=1):
        steps = int(max(abs(x1 - x0), abs(y1 - y0))) * 2 + 1
        for i in range(steps + 1):
            t = i / steps
            x, y = x0 + (x1 - x0) * t, y0 + (y1 - y0) * t
            if width == 1:
                self.set(int(round(x)), int(round(y)), v)
            else:
                self.ellipse(x, y, width / 2, width / 2, v)

    def rect(self, x0, y0, x1, y1, v=INK):
        for y in range(int(y0), int(y1) + 1):
            for x in range(int(x0), int(x1) + 1):
                self.set(x, y, v)

    def arc(self, cx, cy, rx, ry, a0, a1, v=INK, width=1):
        """Elliptical arc, angles in degrees, 0 = right, growing clockwise."""
        import math

        steps = int(max(rx, ry) * 6) + 8
        prev = None
        for i in range(steps + 1):
            a = math.radians(a0 + (a1 - a0) * i / steps)
            x, y = cx + rx * math.cos(a), cy + ry * math.sin(a)
            if prev is not None:
                self.line(prev[0], prev[1], x, y, v, width)
            prev = (x, y)

    @contextmanager
    def clip(self, mode):
        """Restrict painting for the duration of the block.

        `'ink'` paints only over already-lit pixels, which is how the face
        markings stay inside the head: the stripe polygons can run past the
        silhouette and get trimmed to it for free, instead of being fitted to
        the head's curve by hand. `'solid'` paints anywhere inside the badger
        but not on the transparent background.
        """
        prev = self._clip
        self._clip = mode
        try:
            yield self
        finally:
            self._clip = prev

    def mask(self, pred=lambda v: v != CLEAR):
        return [[pred(self.px[y][x]) for x in range(self.w)] for y in range(self.h)]

    def erode_into(self, mask, n, v=DARK):
        """Set the interior of `mask`, `n` pixels in from its edge, to `v`.

        This is what turns a filled silhouette into an outlined one: keep an
        n-pixel rim, hollow out the rest. Cheaper and more even than trying to
        draw the outline directly, and it cannot leave a gap.
        """
        cur = mask
        for _ in range(n):
            nxt = [[False] * self.w for _ in range(self.h)]
            for y in range(self.h):
                for x in range(self.w):
                    if not cur[y][x]:
                        continue
                    if (
                        x == 0
                        or y == 0
                        or x == self.w - 1
                        or y == self.h - 1
                        or not (cur[y][x - 1] and cur[y][x + 1] and cur[y - 1][x] and cur[y + 1][x])
                    ):
                        continue
                    nxt[y][x] = True
            cur = nxt
        for y in range(self.h):
            for x in range(self.w):
                if cur[y][x]:
                    self.px[y][x] = v
        return cur

    def erode(self, mask, n):
        """`mask` shrunk by `n` pixels, 4-connected."""
        cur = mask
        for _ in range(n):
            nxt = [[False] * self.w for _ in range(self.h)]
            for y in range(1, self.h - 1):
                for x in range(1, self.w - 1):
                    if (
                        cur[y][x]
                        and cur[y][x - 1]
                        and cur[y][x + 1]
                        and cur[y - 1][x]
                        and cur[y + 1][x]
                    ):
                        nxt[y][x] = True
            cur = nxt
        return cur

    def outline(self, n=2, v=INK):
        """Force the outer `n` pixels of the silhouette to `v`.

        Run this after the face markings. A stripe that reaches the top of the
        skull would otherwise overwrite the rim: the stripes are clipped to the
        silhouette, and the silhouette's own edge pixels are part of it, so the
        head ends up open against the background. Restoring the rim afterwards is
        one pass that cannot be got wrong -- fitting each stripe inside the curve
        of the skull by hand is four numbers per frame that can.
        """
        m = self.mask()
        inner = self.erode(m, n)
        for y in range(self.h):
            for x in range(self.w):
                if m[y][x] and not inner[y][x]:
                    self.px[y][x] = v

    def rows(self):
        return ["".join(CHARS[v] for v in row) for row in self.px]


# ------------------------------------------------------------------- geometry
#
# One badger, parameterised. Coordinates are in sprite space (x right, y down),
# and every frame differs only in the keyword arguments to `badger()` -- which is
# what keeps eight frames from turning into eight unrelated drawings.
#
# What makes a 1bpp animal read as a *badger* rather than a bear: the head is a
# wedge, not a circle (a wide cranium tapering to a narrow muzzle), the ears are
# small and set low on the sides rather than round on top, and the two dark
# stripes run from the nose up through the eyes to behind the ears, leaving a
# white blaze down the middle and white cheeks outside. The first draft here had
# a circular head with big round ears on top and read unmistakably as a panda.

CRANIUM = dict(cx=36, cy=22, rx=24, ry=15)
JAW = dict(cx=36, cy=34, rx=15, ry=12)
EAR_L = dict(cx=14, cy=15, rx=6, ry=5)
EAR_R = dict(cx=58, cy=15, rx=6, ry=5)
BODY = dict(cx=36, cy=57, rx=27, ry=13)

EYE_L, EYE_R = (25, 21), (47, 21)
EYE_R_PX = 5
NOSE = (36, 36)

# Stripe half-width at the eye. Wide enough to hold an eye -- a badger's stripe
# runs *through* the eye -- and no wider, or the face goes dark and the blaze
# disappears. Every stripe is drawn under `clip("ink")`, so the polygons below
# may overshoot the silhouette and get trimmed to it.
STRIPE = 7


def badger(
    eyes="open",       # open | closed | wide | happy | wink
    mouth="smile",     # smile | flat | open | tongue
    arms="down",       # down | up | dig_lo | dig_hi | hold
    held=None,         # None | "plug" | "disk"
    brows=0,           # -1 worried, 0 neutral, +1 raised
    dirt=0,            # 0/1/2: which spray of dug-up specks, if any
    breathe=0,         # 0/1: body one pixel taller, for the idle cycle
    tilt=0,            # head y offset, for the sleeping frame
    zzz=False,         # sleep bubbles
    sweat=False,       # one bead, for the error frame
):
    c = Canvas()
    cran = dict(CRANIUM, cy=CRANIUM["cy"] + tilt)
    jaw = dict(JAW, cy=JAW["cy"] + tilt)
    body = dict(BODY, ry=BODY["ry"] + breathe, cy=BODY["cy"] - breathe)

    # ---- body first, so the head can be laid over it.
    #
    # Draw the whole lower silhouette solid, then hollow it out: a badger's back
    # and flanks are dark, and a 2px lit rim is what separates them from the
    # background. Doing it by erosion rather than by stroking an outline means
    # the rim cannot develop a gap where two shapes meet.
    c.ellipse(**body, v=INK)
    c.poly([(19, 63), (31, 63), (30, 73), (18, 73)], INK)   # near hind leg
    c.poly([(41, 63), (53, 63), (54, 73), (42, 73)], INK)   # far hind leg
    c.poly([(56, 60), (68, 53), (71, 58), (59, 65)], INK)   # tail
    c.erode_into(c.mask(), 2, DARK)

    # Claws. Along with the face, the thing a badger is actually famous for --
    # lit against the dark paw, so they have to come after the hollowing.
    for px in (21, 24, 27, 45, 48, 51):
        c.line(px, 68, px, 73, INK)

    # ---- head. A dark halo first, painted only where the body already is, so
    # the chin has an edge to sit against instead of melting into the chest.
    # Clipped to "solid" so the halo never leaks onto the background, where it
    # would show up as a dark ring floating around Badgy's neck.
    with c.clip("solid"):
        c.ellipse(cran["cx"], cran["cy"], cran["rx"] + 2, cran["ry"] + 2, DARK)
        c.ellipse(jaw["cx"], jaw["cy"], jaw["rx"] + 2, jaw["ry"] + 2, DARK)

    for e in (EAR_L, EAR_R):
        c.ellipse(e["cx"], e["cy"] + tilt, e["rx"], e["ry"], INK)
    c.ellipse(**cran, v=INK)
    c.ellipse(**jaw, v=INK)
    for e in (EAR_L, EAR_R):
        c.ellipse(e["cx"], e["cy"] + tilt, e["rx"] - 3, e["ry"] - 2, DARK)

    # ---- the stripes: nose, over the eye, to behind the ear. Splayed outward
    # at the top to follow the wedge of the skull.
    ty = cran["cy"] - cran["ry"] - 3
    with c.clip("ink"):
        for sx in (EYE_L[0], EYE_R[0]):
            # Lean, not offset: the band stays centred on the eye and only its
            # top end swings outward to follow the skull. Leaning the whole band
            # walks it off the eye, and an eye half on the stripe reads as a
            # smudge from across a room.
            lean = -2 if sx < 36 else 2
            c.poly(
                [
                    (sx - STRIPE + lean, ty),
                    (sx + STRIPE + lean, ty),
                    (sx + STRIPE - 2, NOSE[1] + tilt),
                    (sx - STRIPE - 1, NOSE[1] + tilt),
                ],
                DARK,
            )

    # The stripes have just eaten the rim wherever they reached the edge of the
    # skull; put it back before anything else is drawn on the face.
    c.outline(2, INK)

    # ---- arms, after the outline pass and on top of everything below the face.
    #
    # Order matters here, and getting it wrong is visible: an arm is 5px wide, so
    # `outline(2)` leaves it with a 1px core and turns the whole limb lit. Every
    # raised arm then reads as a white lump growing out of Badgy's shoulder.
    # Drawn afterwards, a limb keeps its lit body and dark pad.
    def arm(x0, y0, x1, y1):
        # A dark halo first, clipped to the badger so it cannot leak onto the
        # background: without it a lit arm laid over the lit body rim merges with
        # it and Badgy looks like a badger-shaped balloon.
        with c.clip("solid"):
            c.line(x0, y0, x1, y1, DARK, 8)
            c.ellipse(x1, y1, 7, 6, DARK)
        c.line(x0, y0, x1, y1, INK, 5)
        c.ellipse(x1, y1, 5, 4, INK)
        c.ellipse(x1, y1 + 1, 2, 2, DARK)

    if arms == "down":
        arm(19, 52, 13, 62)
        arm(53, 52, 59, 62)
    elif arms == "up":
        arm(19, 52, 11, 40)
        arm(53, 52, 61, 40)
    elif arms == "dig_lo":
        # Both paws stay low and in front. A paw raised to shoulder height reads
        # as a lump on the shoulder at this size, however correct the anatomy --
        # what sells digging is the two paws alternating, plus the spray below.
        arm(20, 52, 16, 67)
        arm(52, 52, 56, 59)
    elif arms == "dig_hi":
        arm(20, 52, 16, 59)
        arm(52, 52, 56, 67)
    elif arms == "hold":
        arm(19, 52, 13, 62)
        arm(52, 50, 59, 45)

    # ---- eyes, over the stripes so there is something to look at. A solid lit
    # eye on a dark stripe needs no outline of its own.
    for (ex, ey), side in ((EYE_L, -1), (EYE_R, +1)):
        ey += tilt
        # A dark socket under every eye, whatever the stripe happens to be doing
        # there. Without it an eye that lands on a white cheek is invisible, and
        # which cheek that is changes every time the stripe geometry is touched.
        with c.clip("solid"):
            c.ellipse(ex, ey, EYE_R_PX + 2, EYE_R_PX + 2, DARK)
        shut = eyes == "closed" or (eyes == "wink" and side < 0)
        if shut:
            c.arc(ex, ey - 1, EYE_R_PX, EYE_R_PX - 2, 20, 160, INK, 2)
        elif eyes == "happy":
            c.arc(ex, ey + 2, EYE_R_PX, EYE_R_PX - 1, 200, 340, INK, 2)
        else:
            r = EYE_R_PX + (1 if eyes == "wide" else 0)
            c.ellipse(ex, ey, r, r, INK)
            pupil = 3 if eyes == "wide" else 2
            # Pupils toed inward: two dots dead centre stare through you, a
            # matched pair aimed slightly in reads as looking *at* something.
            c.ellipse(ex - side, ey + 1, pupil, pupil, DARK)

        if brows:
            # Inner end up for surprise, down for worry.
            inner_y = ey - EYE_R_PX - 4 - 2 * brows
            outer_y = ey - EYE_R_PX - 4
            # Lit, not dark: a brow sits above the eye, which is the middle of a
            # dark stripe. A dark brow drawn there is invisible.
            with c.clip("solid"):
                if side < 0:
                    c.line(ex - 6, outer_y, ex + 6, inner_y, INK, 2)
                else:
                    c.line(ex - 6, inner_y, ex + 6, outer_y, INK, 2)

    # ---- nose and mouth, on the pale muzzle.
    nx, ny = NOSE[0], NOSE[1] + tilt
    c.ellipse(nx, ny, 5, 4, DARK)
    c.ellipse(nx - 1, ny - 1, 2, 1, INK)   # a highlight, or the nose is a hole
    if mouth == "smile":
        c.arc(nx, ny + 3, 5, 4, 25, 155, DARK, 1)
    elif mouth == "flat":
        c.line(nx - 4, ny + 7, nx + 4, ny + 7, DARK, 1)
    elif mouth == "open":
        # Kept inside the jaw: any lower and it merges with the dark halo under
        # the chin, which reads as a spike rather than a mouth.
        c.ellipse(nx, ny + 6, 4, 3, DARK)
    elif mouth == "tongue":
        c.ellipse(nx, ny + 6, 4, 3, DARK)
        c.ellipse(nx, ny + 7, 2, 2, INK)

    # ---- things Badgy holds or emits. Drawn last: they are meant to be read as
    # in front of him.
    if held == "plug":
        # A USB-A plug held up and clear of the head: shell, the tongue inside
        # it, and a lead running down to the raised paw. Anything drawn in front
        # of the chest ends up behind the chin, which reads as Badgy eating it.
        c.rect(57, 25, 69, 37, INK)
        c.rect(59, 27, 67, 35, DARK)
        c.rect(61, 30, 65, 35, INK)      # the tongue
        c.rect(61, 20, 65, 25, INK)      # cable gland
        c.line(63, 37, 60, 44, INK, 2)   # lead down to the paw
    elif held == "disk":
        c.rect(56, 24, 70, 38, INK)
        c.rect(59, 26, 67, 30, DARK)     # label
        c.rect(61, 33, 65, 38, DARK)     # shutter
        c.line(63, 38, 60, 44, INK, 2)

    if dirt:
        # Specks thrown clear of the paws. Two different sprays is what sells the
        # two dig frames as one action instead of two poses.
        spray = ((6, 64), (2, 69), (9, 72), (65, 68), (69, 72)) if dirt == 1 else \
                ((3, 62), (8, 70), (1, 72), (68, 64), (63, 71))
        for (sx, sy) in spray:
            c.ellipse(sx, sy, 1, 1, INK)

    if zzz:
        # Three z's climbing away from the muzzle, biggest last.
        for (zx, zy, s) in ((51, 32, 3), (57, 23, 4), (64, 11, 5)):
            c.line(zx, zy, zx + s, zy, INK)
            c.line(zx + s, zy, zx, zy + s, INK)
            c.line(zx, zy + s, zx + s, zy + s, INK)

    if sweat:
        c.ellipse(68, 10, 3, 4, INK)
        c.ellipse(68, 11, 1, 2, DARK)

    return c
# --------------------------------------------------------------- accessories
#
# Not part of the sheet. This is art for a *script* to inject with `sprite()`,
# emitted as pycon source by `./tools/badger.py pycon`. It lives here rather than
# being typed into the script for the reason at the top of this file -- a mouse
# is two rounded rectangles, and rounded rectangles typed by hand at 1bpp look
# like potatoes.

# Where the USB plug sits in the PLUGGED frame, from `held == "plug"` below:
# shell x 57..69 / y 25..37, cable gland x 61..65 / y 20..25.
PLUG_BOX = (57, 20, 69, 37)
# Where a script pastes the mouse, and how far it bobs between the two frames.
MOUSE_AT = (55, 17)
MOUSE_BOB = 2


def paste(rows, x, y, art):
    """Draw `art` over `rows` at (x, y), spaces transparent.

    The same six lines `samples/jiggle.py` runs on the badge, kept here so the
    mock-ups show what the script actually produces rather than an artist's
    impression of it.
    """
    out = list(rows)
    for i, piece in enumerate(art):
        ry = y + i
        if not (0 <= ry < len(out)):
            continue
        row = list(out[ry])
        for j, ch in enumerate(piece):
            if ch != " " and 0 <= x + j < len(row):
                row[x + j] = ch
        out[ry] = "".join(row)
    return out


def computer_mouse(w=17, h=24):
    """A mouse, top-down, sized to cover the plug in the PLUGGED frame.

    That size is the whole design constraint. A script gets the badger by reading
    a frame back with `badgy_art()`, and the only pose with a paw raised to hold
    something is the one already holding a USB plug -- so this has to be opaque
    everywhere the plug is, or the plug shows around its edges. What it must
    *not* cover is the lead running down from the plug to the paw, which then
    reads as the mouse's cord for free.

    Squared off rather than fully rounded for the same reason: a corner rounded
    enough to look drawn is a corner the plug pokes out of.
    """
    c = Canvas(w, h)
    cx = (w - 1) / 2
    # Palm and buttons as two overlapping ellipses, the lower one wider -- a
    # mouse is a teardrop, narrow where the fingers go.
    c.ellipse(cx, h * 0.62, w / 2, h * 0.40, INK)
    c.ellipse(cx, h * 0.30, w / 2 - 1.5, h * 0.30, INK)
    c.rect(1, h * 0.25, w - 2, h * 0.70, INK)
    c.erode_into(c.mask(), 2, DARK)
    # The split between the buttons, and the wheel sitting in it. Both lit, over
    # the hollowed-out dark body.
    c.line(cx, 2, cx, h * 0.36, INK, 1)
    c.ellipse(cx, h * 0.30, 1.5, 2.5, INK)
    return c


FRAMES = [
    ("IDLE_A", dict()),
    ("IDLE_B", dict(breathe=1, eyes="happy")),
    ("BLINK", dict(eyes="closed")),
    ("SLEEP", dict(eyes="closed", mouth="flat", tilt=2, zzz=True)),
    ("DIG_A", dict(arms="dig_lo", mouth="open", eyes="happy", dirt=1)),
    ("DIG_B", dict(arms="dig_hi", mouth="open", eyes="happy", dirt=2)),
    ("PLUGGED", dict(arms="hold", held="plug", eyes="wide", mouth="smile")),
    ("OOPS", dict(eyes="wide", mouth="open", brows=-1, arms="up", sweat=True)),
]


# ------------------------------------------------------------------- output


def png(path, rows_list, scale=4, gap=4):
    """Write a horizontal sheet of frames. Stdlib only -- zlib and struct."""
    n = len(rows_list)
    fh = len(rows_list[0])
    fw = len(rows_list[0][0])
    sheet_w = (fw + gap) * n * scale
    sheet_h = fh * scale

    # 0 = black background, 255 = ink, 90 = the dark-but-inside-the-badger state,
    # so the three sprite states stay distinguishable in the preview.
    grey = {"#": 255, ".": 60, " ": 0}
    raw = bytearray()
    for y in range(sheet_h):
        raw.append(0)  # PNG filter type 0
        sy = y // scale
        line = bytearray()
        for i, rows in enumerate(rows_list):
            for x in range(fw * scale):
                line.append(grey[rows[sy][x // scale]])
            line.extend(bytes([20] * (gap * scale)))  # separator
        raw.extend(line)

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", sheet_w, sheet_h, 8, 0, 0, 0, 0)  # 8-bit greyscale
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )
    print(f"wrote {path} ({sheet_w}x{sheet_h}, {n} frames of {fw}x{fh})")


RS_HEADER = '''//! Badgy, the BadgyOS badger, as 1bpp sprite frames.
//!
//! GENERATED by `tools/badger.py emit`, but committed and readable on purpose:
//! this is the art, and editing a pixel here with a text editor is a perfectly
//! good way to change it. Re-running the emitter overwrites hand edits, so if
//! you make one, either port it back into the generator or stop running it.
//!
//! Three states per pixel, because Badgy is composited over a live background:
//!
//! | char | meaning   | on the panel                                  |
//! |------|-----------|-----------------------------------------------|
//! | `#`  | ink       | lit (white)                                   |
//! | `.`  | dark      | explicitly black -- occludes the background    |
//! | ` `  | clear     | untouched, outside the silhouette             |
//!
//! The rows live in `.rodata`, which `link.x` maps into FLASH, so a sprite sheet
//! costs image bytes and nothing else -- no `.data`, and so no entries in the
//! 40-slot poke table that `early_init` replays at boot.
//!
//! Preview them with `./tools/badger.py preview`, which renders this file's
//! geometry to `preview/badgy-sheet.png`.

/// A 1bpp sprite: one string per row, `#`/`.`/space as above.
pub struct Sprite {
    pub w: u16,
    pub h: u16,
    pub rows: &'static [&'static str],
}

impl Sprite {
    /// Rows are ASCII by construction, so byte indexing is character indexing.
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        let row = self.rows[y].as_bytes();
        if x < row.len() { row[x] } else { b' ' }
    }
}
'''


def emit_pycon():
    """Print the mouse as pycon source, for pasting into a script.

    Rows are right-trimmed: trailing spaces are the transparent state, and a
    script pasting this treats a short row as one with nothing on the end, so
    the art means the same thing at half the size on the drive.
    """
    rows = computer_mouse().rows()
    print("MOUSE = [")
    for r in rows:
        print(f"    '{r.rstrip()}',")
    print("]")
    print(f"MOUSE_X = {MOUSE_AT[0]}      # over the plug in the BADGY_PLUG frame")
    print(f"MOUSE_Y = {MOUSE_AT[1]}")
    print(f"MOUSE_BOB = {MOUSE_BOB}     # how far it moves between the two frames")


def emit(frames):
    out = [RS_HEADER]
    for name, rows in frames:
        out.append(f"\npub static {name}: Sprite = Sprite {{\n    w: {W},\n    h: {H},\n    rows: &[")
        for r in rows:
            out.append(f'\n        "{r}",')
        out.append("\n    ],\n};\n")

    names = ", ".join(f"&{n}" for n, _ in frames)
    out.append(f"\n/// Every frame, in declaration order. Handy for a sprite-sheet test screen.\npub static ALL: &[&Sprite] = &[{names}];\n")

    path = ROOT / "src" / "sprites.rs"
    path.write_text("".join(out))
    print(f"wrote {path} ({len(frames)} frames of {W}x{H})")


# ------------------------------------------------------- whole-screen mock-up
#
# The sprite is only half the question. What matters is whether Badgy, the title
# and the caption all fit on a 128x128 panel at the offsets `app.rs` actually
# uses. This composites exactly that, with the firmware's own font, so the
# layout can be checked without a badge.

PANEL = 128
BADGY_TOP = 20   # must match src/app.rs
CHAR_W, CHAR_H = 6, 12
FONT_W = 96


def font_glyphs():
    """Decode src/font6x12_1bpp.raw the same way src/gfx.rs walks it."""
    raw = (ROOT / "src" / "font6x12_1bpp.raw").read_bytes()
    per_row = FONT_W // CHAR_W
    glyphs = {}
    for code in range(0x20, 0x7F):
        row, col = divmod(code - 0x20, per_row)
        gx, gy = col * CHAR_W, row * CHAR_H
        bits = []
        for dy in range(CHAR_H):
            v = 0
            for dx in range(CHAR_W):
                i = gx + dx + FONT_W * (gy + dy)
                if raw[i // 8] & (1 << (7 - (i % 8))):
                    v |= 1 << dx
            bits.append(v)
        glyphs[chr(code)] = bits
    return glyphs


def home_screen(rows, title="BadgyOS", caption="-PUSH WHEEL-", rain_seed=0x1337):
    """Composite the home screen as `App::render_splash` draws it."""
    glyphs = font_glyphs()
    c = Canvas(PANEL, PANEL)

    def text(s, y, x0=None):
        x = (PANEL - len(s) * CHAR_W) // 2 if x0 is None else x0
        for i, ch in enumerate(s):
            bits = glyphs.get(ch, glyphs[" "])
            for dy in range(CHAR_H):
                for dx in range(CHAR_W):
                    if bits[dy] & (1 << dx):
                        c.set(x + i * CHAR_W + dx, y + dy, INK)

    # A stand-in for the matrix rain: deterministic, and only here so the mock
    # does not read as "Badgy on a blank screen".
    h = rain_seed
    for _ in range(110):
        h = (h * 1103515245 + 12345) & 0xFFFFFFFF
        text("01<>=+*/$#&"[(h >> 4) % 11], (h >> 20) % PANEL, x0=(h >> 8) % PANEL)

    c.rect(0, 0, PANEL - 1, 15, DARK)
    text(title, 2)
    for y, row in enumerate(rows):
        for x, ch in enumerate(row):
            if ch != " ":
                c.set((PANEL - W) // 2 + x, BADGY_TOP + y, INK if ch == "#" else DARK)
    cap_y = PANEL - CHAR_H - 3
    c.rect(0, cap_y - 2, PANEL - 1, PANEL - 1, DARK)
    text(caption, cap_y)
    return c.rows()


SHOTS = (
    ("IDLE_A", "-PUSH WHEEL-", 0x1337),
    ("DIG_A", "digging...", 0x9E37),
    ("SLEEP", "zzz - any key", 0x51EB),
    ("OOPS", "that went badly", 0x0BAD),
    ("JIGGLE", "jiggling", 0x6199),
)


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "preview"
    built = [(name, badger(**kw).rows()) for name, kw in FRAMES]
    by_name = dict(built)
    # Deliberately not in `built`: this frame is not part of the sheet and must
    # never reach `emit`. It is what a *script* makes at runtime out of a frame
    # that is, and it is here so the mock-ups can show that happening.
    by_name["JIGGLE"] = paste(by_name["PLUGGED"], *MOUSE_AT, computer_mouse().rows())
    if cmd == "preview":
        png(ROOT / "preview" / "badgy-sheet.png", [rows for _, rows in built])
        for name, rows in built:
            png(ROOT / "preview" / f"badgy-{name.lower()}.png", [rows], scale=6)
    elif cmd == "screen":
        png(
            ROOT / "preview" / "badgy-home.png",
            [home_screen(by_name[n], caption=cap, rain_seed=seed) for n, cap, seed in SHOTS],
            scale=3,
            gap=6,
        )
    elif cmd == "docs":
        # Committed, unlike preview/, so the README can show them.
        png(ROOT / "docs" / "badgy-sheet.png", [rows for _, rows in built], scale=3, gap=3)
        png(
            ROOT / "docs" / "badgy-home.png",
            [home_screen(by_name[n], caption=cap, rain_seed=seed) for n, cap, seed in SHOTS],
            scale=3,
            gap=6,
        )
    elif cmd == "emit":
        emit(built)
    elif cmd == "pycon":
        emit_pycon()
    elif cmd == "ascii":
        which = sys.argv[2].upper() if len(sys.argv) > 2 else "IDLE_A"
        print("\n".join(by_name[which]))
    else:
        sys.exit(f"usage: {sys.argv[0]} [preview|screen|docs|emit|pycon|ascii <FRAME>]")


if __name__ == "__main__":
    main()
