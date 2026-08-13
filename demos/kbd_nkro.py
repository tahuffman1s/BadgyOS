# Test: N-key rollover.
#
# The proof is holding more keys at once than a six-key boot report could
# carry, and seeing none of them dropped. On CENTER this presses all ten
# digits of the top row together, holds them for a moment, then lets go.
#
# Point a browser at an online "keyboard tester" (one that shows every key
# currently down), click into it, run this and press CENTER: all ten digits
# should light at once. A plain 6-key keyboard would show only six, or a
# rollover error. key_of() turns each character into its HID keycode; nothing
# is released until the whole row is down, which is the whole test.
#
# Hold LEFT and CENTER to quit.

DIGITS = '1234567890'

rounds = 0

while True:
    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(30, 1, 'NKRO TEST')

    if kbd_ready():
        text(6, 26, 'host: listening')
    else:
        text(6, 26, 'host: none')

    text(6, 50, 'CENTER holds all')
    text(6, 64, '10 digits at once')
    text(6, 88, 'rounds ' + str(rounds))
    text(6, 112, 'L+C quits')
    show()

    if keys() & KEY_CENTER != 0:
        held = 0
        for c in DIGITS:
            if key_press(key_of(c)):
                held = held + 1
        print('holding ' + str(held) + ' keys')
        sleep(700)          # long enough for a tester to catch them all
        key_release_all()
        rounds = rounds + 1
        while keys() & KEY_CENTER != 0:
            sleep(20)

    sleep(40)
