"""frame.py RAW OUT [--logo LOGO X Y WIDTH]: the README's gradient frame."""
import sys
from PIL import Image, ImageDraw, ImageFilter

raw, out = sys.argv[1], sys.argv[2]
win = Image.open(raw).convert("RGBA")
B, R = 15, 16
w, h = win.width + 2 * B, win.height + 2 * B
a, b = (196, 167, 231), (235, 188, 186)  # Rosé Pine iris to rose
grad = Image.new("RGBA", (w, h))
px = grad.load()
for x in range(w):
    t = x / (w - 1)
    c = tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3)) + (255,)
    for y in range(h):
        px[x, y] = c
grad.paste(win, (B, B))
if "--logo" in sys.argv:
    i = sys.argv.index("--logo")
    logo = Image.open(sys.argv[i + 1]).convert("RGBA")
    x, y, lw = (int(v) for v in sys.argv[i + 2 : i + 5])
    logo = logo.resize((lw, round(logo.height * lw / logo.width)), Image.LANCZOS)
    shadow = Image.new("RGBA", logo.size, (0, 0, 0, 0))
    shadow.putalpha(logo.getchannel("A").point(lambda v: v * 0.55))
    pad = 40
    sh = Image.new("RGBA", (logo.width + 2 * pad, logo.height + 2 * pad), (0, 0, 0, 0))
    sh.paste(shadow, (pad, pad))
    sh = sh.filter(ImageFilter.GaussianBlur(14))
    grad.alpha_composite(sh, (B + x - pad + 6, B + y - pad + 10))
    grad.alpha_composite(logo, (B + x, B + y))
mask = Image.new("L", (w, h), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, w - 1, h - 1), R, fill=255)
grad.putalpha(mask)
grad.save(out, optimize=True)
