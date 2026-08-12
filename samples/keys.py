# Shows which keys are held, and how to build a list-driven screen.
#
# keys() returns a bitmask. Test a key with & against one of the KEY_
# constants. All six can be held at once, so this is a loop, not an if/elif.

NAMES = ['WHEEL DN', 'WHEEL IN', 'WHEEL UP', 'LEFT', 'RIGHT', 'CENTER']
BITS = [KEY_DOWN, KEY_SELECT, KEY_UP, KEY_LEFT, KEY_RIGHT, KEY_CENTER]

count = 0

while True:
    held = keys()

    clear()
    rect(0, 0, WIDTH - 1, 13, True)
    text(30, 1, 'KEY WATCH')

    y = 20
    for i in range(len(NAMES)):
        mark = ' . '
        if held & BITS[i] != 0:
            mark = '[#]'
        text(6, y, mark + ' ' + NAMES[i])
        y = y + 12

    text(6, 100, 'mask ' + hex(held))
    text(6, 112, 'L+C quits')
    show()

    # Count how many polls saw at least one key down, just to show that
    # state survives across frames.
    if held != 0:
        count = count + 1

    sleep(30)

print('saw a key on ' + str(count) + ' frames')
