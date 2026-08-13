# Test: modifier chords, picked by the detected OS.
#
# key_tap(code, mods) presses a key with modifiers held and releases the whole
# lot in one call. This types a word, selects all of it with the platform's
# "select all" chord -- Cmd+A on macOS, Ctrl+A everywhere else -- then types
# over the selection. In a text editor you should see the first word replaced
# by the second, once per CENTER press.
#
# Open a text editor, click in, run this, press CENTER. Hold LEFT and CENTER
# to quit.

# Cmd (GUI) on a Mac, Ctrl elsewhere. detect_os() chooses; if it cannot tell,
# Ctrl is the safe default.
if detect_os() == OS_MAC:
    SELECT = MOD_GUI
else:
    SELECT = MOD_CTRL

rounds = 0

while True:
    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(24, 1, 'CHORD TEST')

    if kbd_ready():
        text(6, 26, 'host: listening')
    else:
        text(6, 26, 'host: none')

    text(6, 50, 'CENTER: type,')
    text(6, 64, 'select all, retype')
    text(6, 88, 'rounds ' + str(rounds))
    text(6, 112, 'L+C quits')
    show()

    if keys() & KEY_CENTER != 0:
        type('badgy')
        sleep(80)
        key_tap(key_of('a'), SELECT)   # select all
        sleep(80)
        type('replaced!\n')
        rounds = rounds + 1
        while keys() & KEY_CENTER != 0:
            sleep(20)

    sleep(40)
