# Test: OS detection, by Caps Lock and by what the host asked for.
#
# detect_os() taps Caps Lock and watches whether the host echoes the lock LEDs
# back -- macOS does not, Windows and Linux do -- then splits Windows from Linux
# by a descriptor only Windows asks for while the badge is plugging in. The
# mechanisms are exact; the reading is a heuristic, so treat it as a hint. A PC
# that has seen this badge before skips that question and reads as Linux; a
# fresh usb_id() makes it ask again. The probe puts Caps Lock back the way it
# found it, so your session is left alone.
#
# Press CENTER to probe again. Hold LEFT and CENTER to quit.

NAMES = ['unknown', 'Windows', 'Linux', 'macOS']

guess = detect_os()

while True:
    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(36, 1, 'OS GUESS')

    if kbd_ready():
        text(6, 28, 'host: listening')
    else:
        text(6, 28, 'host: none')

    text(6, 52, 'guess: ' + NAMES[guess])
    text(6, 72, '(a hint, not sure)')
    text(6, 96, 'CENTER re-probes')
    text(6, 112, 'L+C quits')
    show()

    if keys() & KEY_CENTER != 0:
        guess = detect_os()
        while keys() & KEY_CENTER != 0:
            sleep(20)

    sleep(50)
