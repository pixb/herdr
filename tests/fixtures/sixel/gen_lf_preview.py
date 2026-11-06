#!/usr/bin/env python3
"""Generate the captured preview fixtures Herdr's Sixel tests replay.

Fixtures (all committed):

- `lf_preview_source.png`  240x108 deterministic image, the preview subject.
- `lf_preview_frame.six`   real chafa Sixel for that source, written with the exact
  flags the project's `scope` previewer uses for a Herdr pane:
  `chafa -f sixel -s <cols>x<rows> --animate off --polite off --dither none -c 16`
  (`-s` takes a cell count, and chafa's cell unit is 6x10 raster pixels, so the
  preview area's cells are doubled to get the raster chafa emits.)
- `lf_preview_batch.bin`   a complete lf preview batch as a pane PTY receives it:
  lf's surrounding preview text, the Kitty introducer as lf's text-caching
  previewer emits it (the ESC byte arrives as U+FFFD), the full transmission
  metadata, the zlib+base64 payload, the string terminator, trailing text.

Both chafa flags and the U+FFFD introducer shape are observations from live runs
in a Herdr pane, not invented shapes: chafa writes the frame to pane stderr, and
lf caches previewer output as text, which replaces every ESC with U+FFFD.

Run: python3 tests/fixtures/sixel/gen_lf_preview.py
"""

import base64
import subprocess
import zlib
from pathlib import Path

from PIL import Image

HERE = Path(__file__).parent

# 1. deterministic preview source
W, H = 240, 108
img = Image.new("RGB", (W, H))
data = []
for y in range(H):
    for x in range(W):
        r = (x * 255 // W + y * 3 // H) % 256
        g = (y * 255 // H + (x // 30) * 40) % 256
        b = ((x + y) % 60 * 4) % 256
        if (x - 12) % 24 == 0 and (y - 12) % 24 == 0:
            r, g, b = 200, 30, 30
        data.append((r, g, b))
img.putdata(data)
img.save(HERE / "lf_preview_source.png")

# 2. real chafa frame, exact previewer flags, preview area 12x6 cells
out = subprocess.run(
    [
        "chafa",
        "-f",
        "sixel",
        "-s",
        "12x6",
        "--animate",
        "off",
        "--polite",
        "off",
        "--dither",
        "none",
        "-c",
        "16",
        str(HERE / "lf_preview_source.png"),
    ],
    capture_output=True,
)
out.check_returncode()
(HERE / "lf_preview_frame.six").write_bytes(out.stdout)

# 3. live preview batch shape: what one pane PTY read really contains.
#
# lf caches previewer output as text, so the Kitty transmission lf itself emits
# arrives with every ESC replaced by U+FFFD (`\xef\xbf\xbd_G` and `\xef\xbf\xbd\x5c`).
# The previewer's own graphics go to pane *stderr*, which lf does not cache, so the
# chafa Sixel frame keeps its real introducer and terminator.
raw = bytes([200, 30, 30, 255])
payload = base64.b64encode(zlib.compress(raw)).decode()
batch = (
    b"\x1b[1;25l"  # lf's preview control text before the transmission
    b"\xef\xbf\xbd_G"  # introducer as the text cache emits it: U+FFFD `_ G`
    b"a=T,f=100,s=1,v=1,c=1,r=1,q=2;"  # full metadata, first field `a=T`
    + payload.encode()
    + b"\xef\xbf\xbd\x5c"  # terminator as the text cache emits it
    + out.stdout  # the previewer's Sixel frame, straight from pane stderr
    + b"preview done"  # lf's text after the preview
)
(HERE / "lf_preview_batch.bin").write_bytes(batch)

print(
    f"source {W}x{H}, frame {len(out.stdout)} bytes, batch {len(batch)} bytes, "
    f"payload {payload}"
)
