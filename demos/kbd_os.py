# Test: OS detection with the Caps Lock LED trick.
#
# detect_os() taps Caps Lock, watches how -- and how fast -- the host echoes
# the lock LEDs back, and guesses from that. The mechanism is exact; the guess
# is a heuristic: it reads macOS clearly and tells Windows from Linux only
# weakly, so treat the answer as a hint. It puts Caps Lock back the way it
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
