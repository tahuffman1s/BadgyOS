# The badge is a USB keyboard too -- an N-key-rollover one, so it can hold
# any number of keys at once, and it can read the host back: the Num, Caps and
# Scroll lock LEDs are the one thing a host tells a keyboard, and kbd_leds()
# hands them to you.
#
#   kbd_ready()               is a host listening for keystrokes?
#   type(s)                   type an ASCII string (US layout)
#   key_of(s)                 the HID keycode for a printable character
#   key_tap(code [, mods])    press and release a key, with optional modifiers
#   key_press(code [, mods])  hold a key down
#   key_release(code)         let one go
#   key_release_all()         let everything go
#   key_mod(mask)             hold a set of modifiers (MOD_CTRL, MOD_GUI, ...)
#   kbd_leds()                the host's lock LEDs, a mask of LED_CAPS etc.
#   detect_os()               a guess at the host's OS from the Caps Lock trick
#
# This screen shows what the host looks like from here and, on CENTER, types a
# short harmless line into whatever window has focus. Hold LEFT and CENTER to
# quit. Nothing is typed unless you ask for it.

OS_NAMES = ['unknown', 'Windows', 'Linux', 'macOS']

# detect_os() toggles Caps Lock and times how the host echoes the LED back, so
# it is done once up front rather than every frame. It is a hint, not a fact --
# it reads macOS clearly and separates Windows from Linux only weakly.
guess = OS_NAMES[detect_os()]


def leds_line():
    leds = kbd_leds()
    s = 'LEDS'
    if leds & LED_NUM != 0:
        s = s + ' NUM'
    if leds & LED_CAPS != 0:
        s = s + ' CAPS'
    if leds & LED_SCROLL != 0:
        s = s + ' SCROLL'
    return s


typed = 0

while True:
    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(24, 1, 'KEYBOARD')

    if kbd_ready():
        text(6, 20, 'host: listening')
    else:
        text(6, 20, 'host: none')

    text(6, 34, 'os:   ' + guess)
    text(6, 48, leds_line())
    text(6, 70, 'typed ' + str(typed) + ' times')

    text(6, 100, 'CENTER types a line')
    text(6, 112, 'L+C quits')
    show()

    held = keys()
    if held & KEY_CENTER != 0:
        # A chord is one call: hold nothing here, just tap the letters. type()
        # holds Shift itself for the capitals and the punctuation.
        if type('Hello from Badgy! '):
            typed = typed + 1
        # Wait for the key to come back up, so one press is one line.
        while keys() & KEY_CENTER != 0:
            sleep(20)

    sleep(40)
