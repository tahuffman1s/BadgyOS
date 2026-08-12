BadgyOS script drive
====================

Drop .py files here and they appear in the badge's Scripts menu.
Eject the drive, or just leave it alone for a second, and the
badge picks them up -- Badgy digs while a copy is in flight.
Delete a file to remove it.

The badge runs pycon, a subset of Python: ints, strings, bools,
None, lists, if/elif/else, while, for..in, def/return/global,
break/continue, and the usual arithmetic, comparison, boolean
and bitwise operators. There are no classes, imports,
exceptions or floats -- using one is a syntax error, not a
silent surprise.

Drawing -- nothing is visible until you call show():
  clear()                   blank the frame
  pixel(x, y [, on])        one dot; on=False erases
  line(x0, y0, x1, y1)
  rect(x0, y0, x1, y1 [, fill])
  text(x, y, s [, on])      6x12 font, 21 columns of 10 rows;
                            on=False draws dark, for text on
                            top of a filled rectangle
  show()                    push the frame to the panel

Input, timing, output:
  keys()                    bitmask of the keys held right now
  wait_key()                block until a key is pressed
  sleep(ms)
  rand() / rand(n) / rand(lo, hi)
  print(...)                to the serial console, 1 Mbaud

Mouse -- the badge is also a USB mouse on whatever it is plugged
into. Movement is relative, in pixels, clamped to MOUSE_MAX (127)
per call; positive y is down. Each of these returns True if the
host took the report:
  mouse_ready()             is a host listening?
  mouse_move(dx, dy)        nudge the pointer
  mouse_move(dx, dy, w)     ...and the wheel
  mouse_buttons(mask)       hold buttons down; 0 releases
  mouse_click([button])     press and release, default MOUSE_LEFT

This cannot wake a machine that has already gone to sleep, only
keep an awake one from getting there.

USB identity -- what the badge calls itself to the host. Changing
any of these re-presents the device, so this drive blinks out and
comes back:
  usb_vid()                 current vendor id
  usb_pid()                 current product id
  usb_id(pid)               set the product id, keep the vendor
  usb_id(vid, pid)          set both
  usb_name(s)               set the shown name; '' restores default

USB_VID and USB_PID are the badge's own defaults, so a script can
put the identity back. usb_id() refuses one pair -- the
bootloader's -- and returns False for it.

Badgy -- the badger on the home screen is yours to drive. A frame
is named by one int: a BADGY_* mood, or an id that sprite() gave
back. BADGY_AUTO means "whatever he is doing right now", and
handing it to badgy_mood() gives him back to the firmware.
  badgy(x, y [, frame])     draw a frame into your own page
  badgy_art(frame)          his rows, as a list of strings
  sprite(rows)              keep art, return a frame id
  sprite(rows, id)          ...overwriting a frame you own
  badgy_mood(a [, b])       hold him on a, alternating with b
  badgy_say(s)              the line under him; '' gives it back
There is one badger, so the first script to ask for him keeps him
and the others get False -- the same deal as the mouse. He is
handed back when your script ends. Rows are '#' (lit), '.'
(black) and ' ' (see-through), at most SPRITE_MAX_W by
SPRITE_MAX_H, and SPRITE_SLOTS of them are kept at once; sprite()
returns SPRITE_NONE when there is no room, which every call that
takes a frame will quietly answer False to. jiggle.py builds a
badger with a mouse under his paw this way, without drawing a
badger: it paints into what badgy_art() gave it.

Moods: BADGY_IDLE BADGY_BLINK BADGY_SLEEP BADGY_DIG BADGY_PLUG
       BADGY_OOPS

Also available: len str int bool chr ord hex abs min max sum
range; list methods append pop insert remove index count clear
reverse extend copy sort; string methods upper lower strip
startswith endswith find replace split join.

Constants: WIDTH HEIGHT KEY_UP KEY_DOWN KEY_SELECT KEY_LEFT
           KEY_RIGHT KEY_CENTER MOUSE_LEFT MOUSE_RIGHT
           MOUSE_MIDDLE MOUSE_MAX

Hold LEFT and CENTER together to stop a running script.

See hello.py, bounce.py, keys.py, jiggle.py and usbid.py for
working examples.
