# A box bouncing around the panel.
#
# Press any key to stop it. If a script ever stops listening for keys, hold
# LEFT + CENTER together and the badge takes the screen back.

SIZE = 10
x = 20
y = 30
dx = 3
dy = 2
hits = 0

while True:
    clear()
    rect(x, y, x + SIZE, y + SIZE, True)
    text(2, 2, 'hits ' + str(hits))
    show()

    x = x + dx
    y = y + dy
    if x <= 0 or x >= WIDTH - SIZE - 1:
        dx = -dx
        hits = hits + 1
    if y <= 0 or y >= HEIGHT - SIZE - 1:
        dy = -dy
        hits = hits + 1

    if keys() != 0:
        break

print('bounced ' + str(hits) + ' times')
