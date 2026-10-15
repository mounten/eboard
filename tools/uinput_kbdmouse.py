#!/usr/bin/env python3
"""Virtual keyboard + mouse via /dev/uinput (stdlib only). Commands on stdin:
    wait <s> | rel <dx> <dy> | click | key <code> <0|1> | type <text> | wheel <n>

`key` takes Linux key codes (29 = left Ctrl, 30 = A, ...); `type` handles letters and spaces.
Needs root (write access to /dev/uinput).
"""
import fcntl, os, struct, sys, time

EV_SYN, EV_KEY, EV_REL = 0, 1, 2
REL_X, REL_Y, REL_WHEEL = 0, 1, 8
BTN_LEFT = 0x110
KEY_LEFTSHIFT = 42
UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_RELBIT = 0x40045564, 0x40045565, 0x40045566
UI_DEV_SETUP, UI_DEV_CREATE, UI_DEV_DESTROY = 0x405C5503, 0x5501, 0x5502

LETTERS = {c: code for c, code in zip("qwertyuiop", range(16, 26))}
LETTERS.update({c: code for c, code in zip("asdfghjkl", range(30, 39))})
LETTERS.update({c: code for c, code in zip("zxcvbnm", range(44, 51))})
LETTERS[" "] = 57

fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
for ev in (EV_SYN, EV_KEY, EV_REL):
    fcntl.ioctl(fd, UI_SET_EVBIT, ev)
for code in list(range(1, 128)) + [BTN_LEFT]:
    fcntl.ioctl(fd, UI_SET_KEYBIT, code)
for code in (REL_X, REL_Y, REL_WHEEL):
    fcntl.ioctl(fd, UI_SET_RELBIT, code)
fcntl.ioctl(fd, UI_DEV_SETUP, struct.pack("HHHH80sI", 0x03, 1, 2, 1, b"virtual-kbd-mouse", 0))
fcntl.ioctl(fd, UI_DEV_CREATE)

def emit(ty, code, value):
    t = time.time()
    os.write(fd, struct.pack("qqHHi", int(t), int((t % 1) * 1e6), ty, code, value))

def sync():
    emit(EV_SYN, 0, 0)

def key(code, value):
    emit(EV_KEY, code, value); sync(); time.sleep(0.02)

try:
    for line in sys.stdin:
        parts = line.rstrip("\n").split(" ", 1)
        cmd, rest = parts[0], parts[1] if len(parts) > 1 else ""
        if cmd == "wait":
            time.sleep(float(rest))
        elif cmd == "rel":
            dx, dy = map(int, rest.split())
            emit(EV_REL, REL_X, dx); emit(EV_REL, REL_Y, dy); sync()
        elif cmd == "click":
            key(BTN_LEFT, 1); key(BTN_LEFT, 0)
        elif cmd == "wheel":
            emit(EV_REL, REL_WHEEL, int(rest)); sync()
        elif cmd == "key":
            code, value = map(int, rest.split()); key(code, value)
        elif cmd == "type":
            for c in rest:
                shift = c.isupper()
                if shift:
                    key(KEY_LEFTSHIFT, 1)
                key(LETTERS[c.lower()], 1); key(LETTERS[c.lower()], 0)
                if shift:
                    key(KEY_LEFTSHIFT, 0)
finally:
    time.sleep(0.2)
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    os.close(fd)
