# Mouse jiggler -- moves the pointer a little, over and over, so the machine the
# badge is plugged into never decides you have wandered off. You can watch it
# work four ways: the cursor traces a small square and returns about once a
# second; the badge screen shows "sent" climbing; every loop prints a line to the
# serial console (PB14 TX, 1000000 8N1) so you can follow it from a terminal even
# with the badge face-down; and Badgy himself gets a mouse to push, so the home
# screen says what is going on even after you leave this running in the
# background with LEFT + RIGHT.
#
#   WHEEL UP / WHEEL DN   wider / narrower motion
#   WHEEL IN              pause / resume
#   RIGHT                 stats <-> the badger this script is drawing
#   LEFT + CENTER         quit
#
# It cannot wake a machine that is already asleep -- that needs USB remote
# wakeup, which the badge does not claim -- but it keeps an awake one awake.

SIDE = 30       # how far the cursor swings, in pixels; the wheel changes this
SUBSTEP = 6     # pixels per report, so the motion glides instead of jumping
STEP_MS = 15    # gap between reports
REST_MS = 500   # pause between squares

# ---------------------------------------------------------------- the badger
#
# Badgy is 72x74, which is far more art than belongs in a script -- so this does
# not draw a badger. badgy_art() hands back the rows of a frame the badge
# already has, and the only new art here is the mouse. BADGY_PLUG is the frame
# with a paw raised, holding a USB plug up beside his head; pasting a mouse over
# the plug leaves the pose, the paw and the lead, which then reads as the
# mouse's cord. sprite() takes the result back and gives it a frame id, which
# badgy_mood() holds him on until this script lets go or ends.
#
# The mouse is generated: ./tools/badger.py pycon prints these rows, along with
# where to paste them and how far to bob them. It is two rounded shapes, and
# rounded shapes typed by hand at 1bpp look like potatoes.

MOUSE = [
    '     ######',
    '    ########',
    '   ##...#..##',
    '  ##....#...##',
    '  ##....#...##',
    ' ##....##....##',
    ' ##....##.....##',
    ' ##....##.....##',
    ' ##....##.....##',
    ' ##....##.....##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    '##............##',
    ' ##..........##',
    ' ##..........##',
    '  ##........##',
    '   ##########',
    '    ########',
]
MOUSE_X = 55      # over the plug in the BADGY_PLUG frame
MOUSE_Y = 17
MOUSE_BOB = 2     # how far it moves between the two frames


def paste(rows, x, y, art):
    # Draw art over rows at (x, y), in place. A space in the art leaves what is
    # under it alone -- that is what makes this an overlay and not a hole -- and
    # anything that falls outside rows is dropped rather than being an error.
    i = 0
    while i < len(art):
        ry = y + i
        piece = art[i]
        if ry >= 0 and ry < len(rows):
            row = rows[ry]
            out = []
            j = 0
            while j < len(row):
                c = row[j]
                k = j - x
                if k >= 0 and k < len(piece) and piece[k] != ' ':
                    c = piece[k]
                out.append(c)
                j = j + 1
            rows[ry] = ''.join(out)
        i = i + 1


def with_mouse(dy):
    # One frame: the badger holding the mouse, dy pixels down. Returns the frame
    # id, or SPRITE_NONE if there was nowhere to keep it -- which every call that
    # takes a frame will answer False to, so there is nothing to check here.
    rows = badgy_art(BADGY_PLUG)
    if len(rows) == 0:
        return SPRITE_NONE
    paste(rows, MOUSE_X, MOUSE_Y + dy, MOUSE)
    return sprite(rows)


# ---------------------------------------------------------------- the jiggle

paused = False
sent = 0        # reports the host accepted, all-time
loops = 0
last = 0        # keys held on the previous pass
was_live = False
showing = False  # this script's own page: the stats, or the badger
held = False    # whether Badgy is currently ours


def leg(n, dx, dy):
    # One side of the square: n reports in the (dx, dy) direction. Returns how
    # many the host actually accepted, and adds them to the running total.
    global sent
    good = 0
    i = 0
    while i < n:
        if mouse_move(dx, dy):
            sent = sent + 1
            good = good + 1
        sleep(STEP_MS)
        i = i + 1
    return good


def square(side):
    # Four legs that cancel out, so the pointer ends where it began. Returns
    # [accepted, attempted] for this square.
    n = side // SUBSTEP
    if n < 1:
        n = 1
    good = 0
    good = good + leg(n, SUBSTEP, 0)
    good = good + leg(n, 0, SUBSTEP)
    good = good + leg(n, -SUBSTEP, 0)
    good = good + leg(n, 0, -SUBSTEP)
    return [good, 4 * n]


frame_a = with_mouse(0)
frame_b = with_mouse(MOUSE_BOB)

print('jiggle.py: start, mouse_ready=' + str(mouse_ready()))
print('jiggle.py: badgy frames ' + str(frame_a) + ' and ' + str(frame_b))

while True:
    now = keys()
    pressed = now & ~last
    last = now

    if pressed & KEY_UP != 0 and SIDE < 60:
        SIDE = SIDE + 10
    if pressed & KEY_DOWN != 0 and SIDE > 10:
        SIDE = SIDE - 10
    if pressed & KEY_SELECT != 0:
        paused = not paused
        print('jiggle.py: paused=' + str(paused))
    if pressed & KEY_RIGHT != 0:
        showing = not showing

    live = mouse_ready()
    if live != was_live:
        if live:
            print('jiggle.py: host connected')
        else:
            print('jiggle.py: host gone')
        was_live = live

    # Take the mascot while there is jiggling to show, and hand him straight
    # back when there is not -- only on the change, because asking every pass
    # would mean the home screen could never say anything else about the badge
    # while this is running.
    want = live and not paused
    if want != held:
        held = want
        if want:
            if not badgy_mood(frame_a, frame_b):
                print('jiggle.py: another script has Badgy')
            badgy_say('jiggling')
        else:
            badgy_mood(BADGY_AUTO)
            badgy_say('')

    clear()
    if showing:
        # One of the two frames the home screen is alternating, drawn here at
        # full size -- badgy() puts any frame into this script's own page.
        badgy(28, 22, frame_a)
        rect(0, 0, WIDTH - 1, 13, True)
        text(31, 1, 'BADGY VIEW', False)
        text(6, 112, 'right for stats')
    else:
        rect(0, 0, WIDTH - 1, 13, True)
        text(25, 1, 'MOUSE JIGGLER', False)
        if not live:
            text(6, 26, 'usb   no host')
        elif paused:
            text(6, 26, 'usb   paused')
        else:
            text(6, 26, 'usb   jiggling')
        text(6, 42, 'swing ' + str(SIDE) + 'px')
        text(6, 56, 'sent  ' + str(sent))
        text(6, 70, 'loops ' + str(loops))
        text(6, 88, 'wheel size, in=stop')
        text(6, 100, 'right shows badgy')
        text(6, 112, 'L+C quits')
    show()

    if live and not paused:
        res = square(SIDE)
        loops = loops + 1
        # e.g. "loop 12: 20/20 accepted, sent 240"
        print('loop ' + str(loops) + ': ' + str(res[0]) + '/' + str(res[1]) + ' accepted, sent ' + str(sent))
        sleep(REST_MS)
    else:
        sleep(120)
