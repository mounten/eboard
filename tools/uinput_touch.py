#!/usr/bin/env python3
"""Virtual multi-touch screen via /dev/uinput (stdlib only). Reads commands from stdin:

    wait <seconds>
    down <slot> <x> <y>     # finger down (new tracking id)
    move <slot> <x> <y>
    up <slot>
    sync                    # SYN_REPORT
    tap <x> <y>             # down + sync + up + sync on slot 0

Coordinates are panel pixels (native orientation); set W and H for other panel sizes.
Needs root (write access to /dev/uinput), e.g. on the board:

    sudo python3 uinput_touch.py < taps.txt
"""
import fcntl, os, struct, sys, time

W, H = int(os.environ.get("W", 720)), int(os.environ.get("H", 1280))

EV_SYN, EV_KEY, EV_ABS = 0, 1, 3
BTN_TOUCH = 0x14a
ABS_X, ABS_Y = 0, 1
ABS_MT_SLOT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID = 0x2F, 0x35, 0x36, 0x39
INPUT_PROP_DIRECT = 1

UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_ABSBIT, UI_SET_PROPBIT = 0x40045564, 0x40045565, 0x40045567, 0x4004556E
UI_DEV_SETUP, UI_ABS_SETUP = 0x405C5503, 0x401C5504
UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502

fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
for ev in (EV_SYN, EV_KEY, EV_ABS):
    fcntl.ioctl(fd, UI_SET_EVBIT, ev)
fcntl.ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH)
fcntl.ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT)
for code, maximum in ((ABS_X, W - 1), (ABS_Y, H - 1), (ABS_MT_SLOT, 4),
                      (ABS_MT_POSITION_X, W - 1), (ABS_MT_POSITION_Y, H - 1), (ABS_MT_TRACKING_ID, 65535)):
    fcntl.ioctl(fd, UI_SET_ABSBIT, code)
    # struct uinput_abs_setup { u16 code; (pad) struct input_absinfo { value, min, max, fuzz, flat, res } }
    fcntl.ioctl(fd, UI_ABS_SETUP, struct.pack("Hxxiiiiii", code, 0, 0, maximum, 0, 0, 0))
# struct uinput_setup { struct input_id { bustype, vendor, product, version } ; char name[80]; u32 ff_effects_max }
fcntl.ioctl(fd, UI_DEV_SETUP, struct.pack("HHHH80sI", 0x06, 1, 1, 1, b"virtual-touch", 0))
fcntl.ioctl(fd, UI_DEV_CREATE)

def emit(ty, code, value):
    t = time.time()
    os.write(fd, struct.pack("qqHHi", int(t), int((t % 1) * 1e6), ty, code, value))

tracking = 100
def sync():
    emit(EV_SYN, 0, 0)

def down(slot, x, y):
    global tracking
    tracking += 1
    emit(EV_ABS, ABS_MT_SLOT, slot)
    emit(EV_ABS, ABS_MT_TRACKING_ID, tracking)
    emit(EV_ABS, ABS_MT_POSITION_X, x)
    emit(EV_ABS, ABS_MT_POSITION_Y, y)
    emit(EV_KEY, BTN_TOUCH, 1)

def move(slot, x, y):
    emit(EV_ABS, ABS_MT_SLOT, slot)
    emit(EV_ABS, ABS_MT_POSITION_X, x)
    emit(EV_ABS, ABS_MT_POSITION_Y, y)

def up(slot):
    emit(EV_ABS, ABS_MT_SLOT, slot)
    emit(EV_ABS, ABS_MT_TRACKING_ID, -1)

try:
    for line in sys.stdin:
        parts = line.split()
        if not parts or parts[0].startswith("#"):
            continue
        cmd, args = parts[0], [float(a) if "." in a else int(a) for a in parts[1:]]
        if cmd == "wait":
            time.sleep(args[0])
        elif cmd == "down":
            down(*args)
        elif cmd == "move":
            move(*args)
        elif cmd == "up":
            up(*args)
        elif cmd == "sync":
            sync()
        elif cmd == "tap":
            down(0, *args); sync(); time.sleep(0.05); up(0); emit(EV_KEY, BTN_TOUCH, 0); sync()
        print(line.strip(), flush=True)
finally:
    time.sleep(0.2)
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    os.close(fd)
