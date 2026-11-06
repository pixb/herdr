#!/usr/bin/env python3
"""Independent Sixel decoder that turns the captured chafa fixture into raw RGBA ground truth.

This decoder is deliberately *not* Herdr's decoder: it is a separate Python state
machine mirroring foot's `decsixel_generic`, so a test can compare Herdr's decoded
pixels against an independent reading of the same bytes. Run it after regenerating
`lf_preview_frame.six` so the expected pixels stay derived from the real frame:

    python3 tests/fixtures/sixel/gen_lf_preview.py
    python3 tests/fixtures/sixel/gen_lf_preview_expected.py

Single-pass state machine mirroring foot's decsixel_generic:
  #  select color register, optional inline ;2;R;G;B definition
  !  RLE repeat count for the next mask
  < >skip columns
  ?..~ draw mask (bit = ord(c) - 0x3f)
  $  carriage return (col = 0, same row)
  -  GNL (row += 6*pan, col = 0)
RLE runs may push the pen past the DECGRA width; the raster grows to the extent the data reaches, and
the painted height is the band count times six pixels.
Writes raw RGBA ground truth to tests/fixtures/sixel/lf_preview_expected.bin.
"""
from pathlib import Path

HERE = Path(__file__).parent
fixture = HERE / "lf_preview_frame.six"
stream = fixture.read_bytes()
q = stream.index(b"q")
st = stream.index(b"\\", q)
payload = stream[q + 1 : st].decode("latin-1")

pan, pad, ph, pw = 1, 1, 0, 0
palette = {}
color = 0
pending_repeat = 1
pen_x = 0
pen_y = 0
pixels = {}

i = 0
n = len(payload)


def digits(start):
    j = start
    while j < n and (payload[j].isdigit() or payload[j] == ";"):
        j += 1
    return payload[start:j], j


while i < n:
    c = payload[i]
    if c == '"':
        params, i = digits(i + 1)
        parts = [int(p) for p in params.split(";") if p != ""]
        if len(parts) >= 4:
            pan, pad, ph, pw = parts[0], parts[1], parts[2], parts[3]
            pan = min(pan, 5) if pan > 0 else 1
            pad = min(pad, 5) if pad > 0 else 1
        continue
    if c == "#":
        params, i = digits(i + 1)
        parts = params.split(";")
        color = int(parts[0]) if parts and parts[0] else 0
        if len(parts) >= 5 and parts[1] == "2":
            palette[color] = (
                int(parts[2]) * 255 // 100,
                int(parts[3]) * 255 // 100,
                int(parts[4]) * 255 // 100,
            )
        continue
    if c == "!":
        params, i = digits(i + 1)
        pending_repeat = int(params) if params else 1
        continue
    if c == "<":
        params, i = digits(i + 1)
        pen_x -= int(params) if params else 1
        pen_x = max(0, pen_x)
        continue
    if c == ">":
        params, i = digits(i + 1)
        pen_x += int(params) if params else 1
        continue
    if c == "$":
        pen_x = 0
        i += 1
        continue
    if c == "-":
        pen_y += 6 * pan
        pen_x = 0
        i += 1
        continue
    if 0x3F <= ord(c) <= 0x7E:
        mask = ord(c) - 0x3F
        repeat = pending_repeat
        pending_repeat = 1
        bits = [(mask >> b) & 1 for b in range(6)]
        for k in range(repeat):
            x = pen_x + k
            for row, bit in enumerate(bits):
                if bit:
                    y = pen_y + row * pan
                    pixels[(x, y)] = palette.get(color, (0, 0, 0, 255))
        pen_x += repeat
        i += 1
        continue
    i += 1

# height = band count * 6 * pan
bands = payload.count("-") + 1
height = bands * 6 * pan
# Foot grows the image to the pen extent when RLE runs push past the declared
# width, so painting is not clipped at DECGRA width.
width = max(pw, max((x for (x, y) in pixels), default=-1) + 1)

out = bytearray(width * height * 4)
for (x, y), (r, g, b) in sorted(pixels.items()):
    if x < width and y < height:
        j = (y * width + x) * 4
        out[j : j + 4] = bytes([r, g, b, 255])

(HERE / "lf_preview_expected.bin").write_bytes(bytes(out))
print("width", width, "height", height, "painted", len(pixels), "palette", len(palette))
