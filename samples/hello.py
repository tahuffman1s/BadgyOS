# The smallest useful script: draw something, wait for a key.
# Nothing reaches the panel until show() is called.

clear()
text(28, 10, 'HELLO BADGY')
rect(4, 30, WIDTH - 5, 52, False)
text(10, 36, 'press any key')

# A row of dots along the middle, counted as we go.
n = 0
for i in range(0, WIDTH, 8):
    pixel(i, 70)
    n = n + 1
text(4, 84, str(n) + ' dots')

show()
wait_key()

# print() goes to the serial console, not the panel.
print('hello.py finished')
