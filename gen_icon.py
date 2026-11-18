#!/usr/bin/env python3
"""Generate SlotShift.icns from scratch (orange circle icon, #D97757)."""
import struct, zlib, os, subprocess, sys

def make_png(size):
    pixels = []
    center = size / 2.0
    for y in range(size):
        row = b'\x00'
        for x in range(size):
            dx = x - center + 0.5
            dy = y - center + 0.5
            dist = (dx*dx + dy*dy)**0.5
            radius = size * 0.35
            if dist <= radius - 0.5:
                row += bytes([0xD9, 0x77, 0x57, 0xFF])
            elif dist <= radius:
                a = int(0xFF * (1 - (dist - radius + 0.5)))
                row += bytes([0xD9, 0x77, 0x57, max(0, min(255, a))])
            else:
                row += b'\x00\x00\x00\x00'
        pixels.append(row)

    raw = b''.join(pixels)
    def chunk(ctype, data):
        c = ctype + data
        return struct.pack('>I', len(data)) + c + struct.pack('>I', zlib.crc32(c) & 0xffffffff)
    ihdr = struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')

iconset = 'SlotShift.iconset'
os.makedirs(iconset, exist_ok=True)
mapping = [
    ('icon_16x16.png', 16), ('icon_16x16@2x.png', 32),
    ('icon_32x32.png', 32), ('icon_32x32@2x.png', 64),
    ('icon_128x128.png', 128), ('icon_128x128@2x.png', 256),
    ('icon_256x256.png', 256), ('icon_256x256@2x.png', 512),
    ('icon_512x512.png', 512), ('icon_512x512@2x.png', 1024),
]
for name, size in mapping:
    with open(os.path.join(iconset, name), 'wb') as f:
        f.write(make_png(size))

subprocess.run(['iconutil', '-c', 'icns', iconset, '-o', 'SlotShift.icns'], check=True)
print('SlotShift.icns created.')
