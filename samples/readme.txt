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

Keyboard -- the badge is also a USB keyboard, with N-key rollover,
so it can hold any number of keys at once. Codes are HID keycodes;
type() maps ASCII for you. Each of these returns True if the host
took the report:
  kbd_ready()               is a host listening?
  type(s)                   type an ASCII string (US layout)
  key_of(s)                 the HID keycode for a printable char
  key_tap(code [, mods])    press and release, with modifiers
  key_press(code [, mods])  hold a key down
  key_release(code)         let one go; key_release_all() lets all
  key_mod(mask)             hold a set of modifiers
  kbd_leds()                the host's lock LEDs, a mask of LED_*
  detect_os()               guess the host OS, an OS_* value

detect_os() plays the Caps Lock LED trick -- toggle a lock key and
watch how the host echoes it back -- so it is a hint, not a fact:
it reads macOS clearly and splits Windows from Linux only weakly.
kbd_leds() is the readback, the one thing a host tells a keyboard.
A chord is one call: key_tap(key_of("r"), MOD_GUI) is Win+R, and
key_tap(KEY_DELETE, MOD_CTRL | MOD_ALT) is Ctrl+Alt+Del. Keys the
script is still holding are released when it ends.

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
badger with a mouse in his paw this way, without drawing a
badger: it paints into what badgy_art() gave it. Nothing the
firmware does outranks your hold, so keep him for as long as you
are running and let the pose say what you are up to.

Moods: BADGY_IDLE BADGY_BLINK BADGY_SLEEP BADGY_DIG BADGY_PLUG
       BADGY_OOPS

Also available: len str int bool chr ord hex abs min max sum
range; list methods append pop insert remove index count clear
reverse extend copy sort; string methods upper lower strip
startswith endswith find replace split join.

Constants: WIDTH HEIGHT KEY_UP KEY_DOWN KEY_SELECT KEY_LEFT
           KEY_RIGHT KEY_CENTER MOUSE_LEFT MOUSE_RIGHT
           MOUSE_MIDDLE MOUSE_MAX

Keyboard constants: MOD_CTRL MOD_SHIFT MOD_ALT MOD_GUI (and the
right-hand MOD_RCTRL MOD_RSHIFT MOD_RALT MOD_RGUI); LED_NUM
LED_CAPS LED_SCROLL; OS_UNKNOWN OS_WINDOWS OS_LINUX OS_MAC; and
named keys KEY_ENTER KEY_ESC KEY_BACKSPACE KEY_TAB KEY_SPACE
KEY_CAPSLOCK KEY_F1..KEY_F12 KEY_INSERT KEY_HOME KEY_PAGEUP
KEY_DELETE KEY_END KEY_PAGEDOWN and KEY_UP_ARROW KEY_DOWN_ARROW
KEY_LEFT_ARROW KEY_RIGHT_ARROW (suffixed so they do not clash
with the d-pad's own KEY_UP and friends).

Hold LEFT and CENTER together to stop a running script.

See hello.py, bounce.py, keys.py, jiggle.py, usbid.py and
keyboard.py for working examples.
