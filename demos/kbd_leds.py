# Test: host lock-LED readback.
#
# A USB keyboard hears exactly one thing back from the host -- the Num, Caps
# and Scroll lock LEDs -- and kbd_leds() hands you that byte. Run this, then
# toggle those locks on your *real* keyboard and watch this screen follow.
# That round trip is the same one detect_os() times.
#
# Hold LEFT and CENTER together to quit.


def box(mask, bit):
    if mask & bit != 0:
        return '[#]'
    return '[ ]'


while True:
    leds = kbd_leds()

    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(18, 1, 'LOCK LEDS')

    text(10, 26, box(leds, LED_NUM) + ' NUM')
    text(10, 42, box(leds, LED_CAPS) + ' CAPS')
    text(10, 58, box(leds, LED_SCROLL) + ' SCROLL')

    if kbd_ready():
        text(10, 86, 'host: listening')
    else:
        text(10, 86, 'host: none')

    text(6, 112, 'L+C quits')
    show()
    sleep(50)
