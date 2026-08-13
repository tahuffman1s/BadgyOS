# Test: typing, with type()'s ASCII mapping and Shift handling.
#
# The line below has lower and upper case, digits and shifted punctuation, so
# it exercises the whole US-layout table -- and Shift is held only across the
# runs that need it, not toggled per key. Open a text editor, click into it,
# run this, and press CENTER. The line should appear exactly, once per press.
#
# Nothing is typed unless you ask for it. Hold LEFT and CENTER to quit.

LINE = 'BadgyOS keyboard test: Hello, World! 0123456789 <>?{}|'

sent = 0

while True:
    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(30, 1, 'TYPE TEST')

    if kbd_ready():
        text(6, 26, 'host: listening')
    else:
        text(6, 26, 'host: none')

    text(6, 50, 'CENTER types a')
    text(6, 64, 'test line + enter')
    text(6, 88, 'sent ' + str(sent) + ' times')
    text(6, 112, 'L+C quits')
    show()

    if keys() & KEY_CENTER != 0:
        # A trailing newline presses Enter, so each press is its own line.
        if type(LINE + '\n'):
            sent = sent + 1
        # Wait for the button to come back up, so one press is one line.
        while keys() & KEY_CENTER != 0:
            sleep(20)

    sleep(40)
