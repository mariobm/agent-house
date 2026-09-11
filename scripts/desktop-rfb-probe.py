#!/usr/bin/env python3
"""Bounded RFB 3.8 smoke client for the private desktop socket."""
import socket
import struct
import sys
import time
import zlib
from pathlib import Path

s = socket.socket(socket.AF_UNIX)
s.settimeout(10)
s.connect(sys.argv[1])

def read(n):
    data = bytearray()
    while len(data) < n:
        part = s.recv(n - len(data))
        if not part:
            raise RuntimeError('VNC disconnected')
        data.extend(part)
    return bytes(data)

assert read(12) == b'RFB 003.008\n'
s.sendall(b'RFB 003.008\n')
security = read(read(1)[0])
assert 1 in security, security  # Unauthenticated only behind a private Unix socket.
s.sendall(b'\x01')
assert read(4) == bytes(4)
s.sendall(b'\x01')
w, h = struct.unpack('!HH', read(4))
assert 0 < w <= 1920 and 0 < h <= 1080, (w, h)
read(16)
name_len = struct.unpack('!I', read(4))[0]
assert name_len < 4096
print('VNC desktop:', read(name_len).decode(), w, h, flush=True)
# Explicit little-endian 32bpp RGB, raw rectangles only.
s.sendall(b'\x00\x00\x00\x00' + struct.pack('!BBBBHHHBBBxxx', 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
s.sendall(struct.pack('!BBHi', 2, 0, 1, 0))


def capture(path):
    s.sendall(struct.pack('!BBHHHH', 3, 0, 0, 0, w, h))
    pixels = bytearray(w * h * 4)
    covered = 0
    while covered < w * h:
        kind = read(1)[0]
        if kind == 2:  # Bell
            continue
        if kind == 3:  # Clipboard
            read(3)
            length = struct.unpack('!I', read(4))[0]
            assert length < 1024 * 1024
            read(length)
            continue
        assert kind == 0, kind
        read(1)
        for _ in range(struct.unpack('!H', read(2))[0]):
            x, y, rw, rh, encoding = struct.unpack('!HHHHi', read(12))
            assert encoding == 0 and x + rw <= w and y + rh <= h
            data = read(rw * rh * 4)
            for row in range(rh):
                dest = ((y + row) * w + x) * 4
                pixels[dest:dest + rw * 4] = data[row * rw * 4:(row + 1) * rw * 4]
            covered += rw * rh
    rgb = bytearray()
    for row in range(h):
        rgb.append(0)
        for col in range(w):
            i = (row * w + col) * 4
            rgb.extend((pixels[i + 2], pixels[i + 1], pixels[i]))
    def chunk(tag, data):
        return struct.pack('!I', len(data)) + tag + data + struct.pack('!I', zlib.crc32(tag + data))
    Path(path).write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(rgb)) + chunk(b'IEND', b''))
    return pixels

for attempt in range(5):
    before = capture(sys.argv[2] + '-before.png')
    if len(set(before)) > 16:
        break
    time.sleep(0.3)
else:
    raise RuntimeError('VNC kept returning a blank framebuffer')
# Move/click within the terminal, then type a command through VNC key events.
s.sendall(struct.pack('!BBHH', 5, 0, 320, 240))
s.sendall(struct.pack('!BBHH', 5, 1, 320, 240))
s.sendall(struct.pack('!BBHH', 5, 0, 320, 240))
command = sys.argv[3] if len(sys.argv) > 3 else 'echo AHVM-VNC-INPUT-OK | tee /home/desktop/input-ok'
for key in [ord(c) for c in command] + [0xff0d]:
    s.sendall(struct.pack('!BBHI', 4, 1, 0, key) + struct.pack('!BBHI', 4, 0, 0, key))
    time.sleep(0.01)
time.sleep(1)
after = capture(sys.argv[2] + '-after.png')
assert before != after, 'Input did not change the captured desktop'
print('VNC_CAPTURE_OK; keyboard and pointer sent (guest verifies delivery)', flush=True)
s.close()
