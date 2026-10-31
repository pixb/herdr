#!/usr/bin/env python3
"""Generate a realistic Sixel test frame mimicking chafa/img2sixel output.

Data characters are 6-row vertical bitmasks (bit0 = top row); the pixel color
is the current color index set by `#N` (DEC default palette). This is exactly
how libsixel/chafa encode: `#N` selects the palette entry, then `!M~` paints
M solid columns of 6 rows in that color, switching colors as needed.

Output: tests/fixtures/sixel/rainbow_palette.six  (payload only, no DCS
wrapper -- the extractor strips the DCS intro before decode).
"""

from pathlib import Path

DEC_BASE = [
    (0, 0, 0),          # 0 black
    (51, 51, 204),      # 1 blue
    (204, 33, 33),      # 2 red
    (51, 204, 51),      # 3 green
    (204, 51, 204),     # 4 magenta
    (51, 204, 204),     # 5 cyan
    (204, 204, 51),     # 6 yellow
    (135, 135, 135),    # 7 gray 50%
    (66, 66, 66),       # 8 gray 25%
    (84, 84, 153),      # 9 blue (bright)
    (153, 66, 66),      # 10 red (bright)
    (84, 153, 84),      # 11 green (bright)
    (153, 84, 153),     # 12 magenta (bright)
    (84, 153, 153),     # 13 cyan (bright)
    (153, 153, 84),     # 14 yellow (bright)
    (204, 204, 204),    # 15 gray 75%
]
CUBE_STEP = [0, 51, 102, 153, 204, 255]
GRAY = [(i * 11,) * 3 for i in range(24)]  # 0,11,...,253

def palette():
    p = list(DEC_BASE)
    for r in CUBE_STEP:
        for g in CUBE_STEP:
            for b in CUBE_STEP:
                p.append((r, g, b))
    p += GRAY
    return p

SOLID = chr(0x3F + 63)  # 6 rows all set: a solid band column.

def encode_row(row, current):
    """row: list of palette indices for one 128-col scanline."""
    out = []
    i = 0
    while i < len(row):
        j = i
        while j < len(row) and row[j] == row[i]:
            j += 1
        run = j - i
        if row[i] != current:
            out.append("#%d" % row[i])
            current = row[i]
        if run > 1:
            out.append("!%d" % run)
        out.append(SOLID)
        i = j
    return "".join(out), current

def main():
    pal = palette()
    # 24 bands x 128 cols. Sweep dark/bright/cube/grayscale across the bands
    # so the frame exercises the whole default palette.
    bands = []
    for band in range(24):
        row = []
        for col in range(128):
            if band < 8:
                row.append(1 + (col % 7))
            elif band < 16:
                row.append(8 + (col % 8))
            elif band < 20:
                row.append(16 + (col % 216))
            else:
                row.append(232 + ((col * 3 + band) % 24))
        bands.append(row)

    lines = ["#0;2;0;0", '"1;1;128;144', "#0"]
    current = 0
    for band, row in enumerate(bands):
        encoded, current = encode_row(row, current)
        lines.append(encoded)
        if band < len(bands) - 1:
            lines.append("$")  # carriage return to column 0
            lines.append("-")  # next band
    payload = "\r\n".join(lines).encode("ascii")
    out = Path(__file__).parent / "rainbow_palette.six"
    out.write_bytes(payload)
    print("wrote %s: %d bytes" % (out, len(payload)))

if __name__ == "__main__":
    main()
