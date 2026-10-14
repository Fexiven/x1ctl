#!/usr/bin/env python3
"""Simulated Sound Blaster X1 on a pseudo-terminal, for testing x1d/x1ctl without hardware.

Behaviour follows USB captures of a real X1 (fw 1.0.220525.1100): text-mode
whoareyou/unlock (AES-256-GCM, same key schedule as the real device), SW_MODE1,
then binary 5A frames. Unsupported opcodes get `5A 02 0A <op> 81 …`.

Usage: fake_x1.py <symlink-path>   (prints nothing; creates the symlink to the pty)
Send SIGUSR1 to simulate pressing the physical Super X-Fi button,
SIGUSR2 to simulate a volume knob click.
"""
import os, pty, sys, struct, signal, tty, select
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

KEY_DATA = bytes([0xD3,0x1A,0x21,0x27,0x9B,0xE3,0x46,0xF0,0x99,0x9D,0x6E,0xC4,0xC3,0xFE,0xBE,0x98,
                  0x90,0x18,0x69,0xC1,0x18,0xFB,0xB1,0x25,0x6E,0x0C,0xE0,0x7B])
HDR, PID = b"\x1e\x04", b"\x86\x32"

master, slave = pty.openpty()
tty.setraw(slave)
link = sys.argv[1]
try: os.unlink(link)
except FileNotFoundError: pass
os.symlink(os.ttyname(slave), link)

mode = "text"
unlocked = False
nonce = None
sxfi = False
eq = {False: {0x09: 1.0, 0x0A: 0.0, **{0x0B+i: float(i-5) for i in range(10)}},
      True:  {0x09: 1.0, 0x0A: -2.0, **{0x0B+i: 0.0 for i in range(10)}}}
level = -3080
muted = 0
pending = []
log = open(link + ".log", "w", buffering=1)

def out(b): os.write(master, b)
def frame(op, payload): return bytes([0x5A, op, len(payload)]) + bytes(payload)
def ack(op, status=0, echo=b""): return frame(0x02, bytes([op, status]) + (echo + b"\0"*8)[:8])
def dump():
    items = sorted(eq[sxfi].items())
    a = b"".join(bytes([0x96, i]) + struct.pack("<f", v) for i, v in items[:9])
    b = b"".join(bytes([0x96, i]) + struct.pack("<f", v) for i, v in items[9:])
    return frame(0x11, bytes([9, 1]) + a) + frame(0x11, bytes([3, 0]) + b)
def sxfi_state(): return frame(0x26, bytes([0x08, 0xFF, 0xFF, int(sxfi)]) + b"\0"*7)

def toggle_sequence(echo_ack=None):
    s = sxfi_state() + frame(0x1A, bytes([2, int(sxfi)]))
    if echo_ack: s += echo_ack
    return s + dump()

def on_usr1(*_):
    global sxfi
    sxfi = not sxfi
    pending.append(toggle_sequence() + sxfi_state())
def on_usr2(*_):
    global level
    level -= 440   # one knob click = 440/256 dB on the real X1
    pending.append(frame(0x23, bytes([0]) + struct.pack("<h", level)))
signal.signal(signal.SIGUSR1, on_usr1)
signal.signal(signal.SIGUSR2, on_usr2)

def handle_text(line):
    global unlocked, nonce, mode
    log.write(f"TEXT {line!r}\n")
    if line.startswith(b"whoareyou"):
        if unlocked: out(b"Unknown command\r\n"); return
        nonce = os.urandom(30) + b"\r\n"   # deliberately contains CRLF
        out(b"whoareyou" + HDR + PID + nonce + b"\r\n")
    elif line.startswith(b"unlock"):
        r = line[6:6+64]
        try:
            pt = AESGCM(HDR + KEY_DATA + PID).decrypt(r[:12], r[16:64], None)
            ok = pt == nonce
        except Exception: ok = False
        unlocked = ok
        out(b"unlock_OK\r\n" if ok else b"unlock_FAIL\r\n")
    elif line.startswith(b"SW_MODE1"):
        if not unlocked: out(b"Unknown command\r\n"); return
        mode = "bin"
        out(bytes.fromhex("5B 02 0A 00 6D 00 00 00 00 00 00 00 00 00"))
    elif line.strip():
        out(b"Unknown command\r\n")

def handle_frame(op, p):
    global sxfi, level, muted
    log.write(f"BIN {op:02X} {p.hex(' ')}\n")
    if op == 0x03: out(frame(0x03, b"\x00\x02"))
    elif op == 0x09:
        if p == b"\x00": out(frame(0x09, bytes.fromhex("00 86 32 00 01 1E 04") + b"\0"*10))
        elif p == b"\x01\x00": out(frame(0x09, bytes.fromhex("01 00 60 E5 32 20 2D 04")))
        elif p == b"\x02": out(frame(0x09, b"\x02\x10" + b"1.0.220525.1100\0"))
        elif p == b"\x03": out(frame(0x09, b"\x03" + b"W6ZZ0090SIMULATEDX\0"))
        else: out(ack(op, 0x81))
    elif op == 0x10: out(frame(0x10, bytes.fromhex("40 00 00 00 00 00 00 00")))
    elif op == 0x11:
        i = p[2]; out(frame(0x11, bytes([1, 0, 0x96, i]) + struct.pack("<f", eq[sxfi][i])))
    elif op == 0x12:
        i = p[2]; v = struct.unpack("<f", p[3:7])[0]; eq[sxfi][i] = v
        out(ack(0x12))
        if i == 0x09: out(frame(0x11, bytes([1, 0, 0x96, 9]) + struct.pack("<f", v)))
    elif op == 0x1A: out(frame(0x1A, bytes([2, int(sxfi)])))
    elif op == 0x1D: out(frame(0x1D, bytes.fromhex("01 00 02 00 01 01 36 4C 8C 00")))
    elif op == 0x21: out(frame(0x21, bytes.fromhex("02 02 00 00 C2 09 C3 0A 01 00 00 00")))
    elif op == 0x22: out(frame(0x22, bytes.fromhex("02 00 00 00 00 28 CE C5 01 01 00 00 00 D0 00 01 00 00 00 D0 00 01")))
    elif op == 0x23:
        if p[0] == 1: out(frame(0x23, bytes([p[1]]) + struct.pack("<h", level if p[1] == 0 else -3072)))
        else:
            v = struct.unpack("<h", p[2:4])[0]
            level = -12760 if v > 2200 else -((-v) // 440) * 440   # rounds up to a 440 step; above max: silent
            out(ack(0x23))
    elif op == 0x24:
        if p[0] == 1: out(frame(0x24, bytes([p[1], muted if p[1] == 0 else 0])))
        else: muted = p[2]; out(ack(0x24))
    elif op == 0x26:
        if p == b"\x05": out(frame(0x26, bytes.fromhex("05 00 04 00 1E 00 0A 40 0B 40 0C 40")))
        elif p[:1] == b"\x08": out(sxfi_state())
        elif p[:1] == b"\x07":
            sxfi = bool(p[3])
            out(toggle_sequence(echo_ack=ack(0x26, 0, bytes([7, 0x1E, 0, p[3], 0]))))
        else: out(ack(op, 0x83))
    elif op == 0x39:
        if p == b"\x02": out(frame(0x39, bytes.fromhex("02 00 00 00 04 00 00 00 00")))
        elif p[:1] == b"\x01": out(frame(0x39, bytes.fromhex("01 00 00 00 00")))
        else: out(ack(op, 0x00))
    elif op == 0x6D: out(frame(0x6D, b"\x01\x00"))
    elif op == 0x09 and p == b"\x01\x00": pass  # handled below (kept for clarity)
    elif op == 0x9B:
        if p == b"\x02": out(ack(0x9B, 0x81))   # the real X1 rejects the support query
        elif p == b"\x01":
            out(ack(0x9B))
            sxfi = False
            for slot in (False, True):
                eq[slot].update({0x09: 1.0, 0x0A: 0.0, **{0x0B+i: 0.0 for i in range(10)}})
            level = -6600; muted = 0
            out(sxfi_state() + frame(0x1A, bytes([2, 0])) + dump())
        else: out(ack(op, 0x83))
    elif op == 0x6F:
        if p == b"\x00": out(frame(0x6F, bytes.fromhex("00 03 01 07 02 01 1C 01")))
        elif p == b"\x06": out(frame(0x6F, b"\x06SIM-HP-0001\0profile.sxfi\0"))
        else: out(ack(op, 0x81))
    else:
        out(ack(op, 0x81))

buf = b""
while True:
    while pending: out(pending.pop(0))
    try:
        r, _, _ = select.select([master], [], [], 0.1)
    except InterruptedError:
        continue
    if not r: continue
    try: data = os.read(master, 4096)
    except OSError: continue
    buf += data
    while buf:
        if mode == "text":
            if b"\r\n" not in buf: break
            # an unlock line carries 64 binary bytes that may contain CRLF
            if buf.startswith(b"unlock") and len(buf) < 6 + 64 + 2: break
            if buf.startswith(b"unlock"):
                line, buf = buf[:72], buf[72:]
            else:
                i = buf.index(b"\r\n"); line, buf = buf[:i], buf[i+2:]
            if line.startswith(b"\x5a\x03\x00"):   # probe while in text mode: ignored
                line = line[3:]
            handle_text(line)
        else:
            j = min([k for k in (buf.find(b"\x5a"),) if k >= 0], default=-1)
            if j < 0: buf = b""; break
            buf = buf[j:]
            if len(buf) < 3 or len(buf) < 3 + buf[2]: break
            op, n = buf[1], buf[2]
            p, buf = buf[3:3+n], buf[3+n:]
            handle_frame(op, p)
