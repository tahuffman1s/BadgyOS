# USB identity -- the badge can tell the host it is plugged into that it is
# some other device entirely. Vendor id, product id and the name shown in a
# device list are all just values you set from here, the same way keys() reads
# a button or mouse_move() nudges the pointer: call the subsystem, get a result.
#
# Changing any of them re-presents the device to the host, so the drive you
# dropped this file on will blink out and come back. That is the only way a
# host that already saw the badge will notice a new identity.
#
#   usb_vid()            -> current vendor id
#   usb_pid()            -> current product id
#   usb_id(pid)          set product id, keep the vendor
#   usb_id(vid, pid)     set both
#   usb_name(str)        set the shown name ('' restores the default)
#
# USB_VID and USB_PID are the badge's own defaults, so a script can always put
# things back the way it found them -- which this one does at the end.


def label(tag):
    print(tag + ' ' + hex(usb_vid()) + ':' + hex(usb_pid()))


label('before')

# Any pair works. 0x1209 is pid.codes, the vendor id set aside for open-source
# and hobby projects, so this is an honest "test device" rather than a
# stand-in for someone else's hardware. Swap in whatever you are testing
# against on your own equipment.
usb_name('Badgy Says Hi')
applied = usb_id(0x1209, 0x0001)
label('after')
print('applied ' + str(applied))

# The one identity the badge will not take: the bootloader's own id, because
# the flashing tool finds the bootloader by matching it exactly.
guarded = usb_id(0x1d50, 0x6196)
print('bootloader id refused: ' + str(not guarded))

# Put it back. An empty name restores the default product string.
usb_name('')
usb_id(USB_VID, USB_PID)
label('restored')
