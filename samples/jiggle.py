# Mouse jiggler -- moves the pointer a little, over and over, so the machine the
# badge is plugged into never decides you have wandered off. You can watch it
# work three ways: the cursor traces a small square and returns about once a
# second; the badge screen shows "sent" climbing; and every loop prints a line
# to the serial console (PB14 TX, 1000000 8N1) so you can follow it from a
# terminal even with the badge face-down.
#
#   WHEEL UP / WHEEL DN   wider / narrower motion
#   WHEEL IN              pause / resume
#   LEFT + CENTER         quit
#
# It cannot wake a machine that is already asleep -- that needs USB remote
# wakeup, which the badge does not claim -- but it keeps an awake one awake.

SIDE = 30       # how far the cursor swings, in pixels; the wheel changes this
SUBSTEP = 6     # pixels per report, so the motion glides instead of jumping
STEP_MS = 15    # gap between reports
REST_MS = 500   # pause between squares

paused = False
sent = 0        # reports the host accepted, all-time
loops = 0
last = 0        # keys held on the previous pass
was_live = False


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


print('jiggle.py: start, mouse_ready=' + str(mouse_ready()))

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

    live = mouse_ready()
    if live != was_live:
        if live:
            print('jiggle.py: host connected')
        else:
            print('jiggle.py: host gone')
        was_live = live

    clear()
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
    text(6, 100, 'wheel size, in=stop')
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
