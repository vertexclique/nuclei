#!/usr/bin/env python3
"""Draws the Nuclei mark and writes every logo file from that one drawing.

Rebuild all of them, from the repository root, with:

    python3 art/build.py

The mark sits on a 1024 grid centred on (512, 512): a diamond inside a larger diamond, and two
arm units, one along an axis (an arrow and the chevron beyond it) and one along a diagonal (an
arrow and the corner bracket beyond it), each placed four times by rotation about the centre.
The diagonal arrow carries a hook on one side, so the mark turns one way: it is symmetric under
the four rotations, as the original drawing is. Every line has the same width, and every colour
is a linear gradient from turquoise and cyan through blue to violet.

It writes the SVG files with no comments and no metadata, renders the PNG files from them with
rsvg-convert, and keeps only the image chunks of each PNG (no text, time, EXIF or colour
profile). Files written:

    art/nuclei-logo.svg              the mark on a transparent background, cropped to it
    art/nuclei-logo-dark.svg         the mark on its navy square
    art/nuclei-logo-512.png          nuclei-logo.svg at 512 by 512
    art/nuclei-logo-1024.png         nuclei-logo.svg at 1024 by 1024
    art/nuclei-logo-square.png       nuclei-logo-dark.svg at 1024 by 1024
    art/nuclei-logo-square-512.png   nuclei-logo-dark.svg at 512 by 512
    site/favicon.svg                 the favicon: the mark, heavier still, on a navy square
    site/favicon-32.png              site/favicon.svg at 32 by 32, for browsers without SVG icons
    site/apple-touch-icon.png        site/favicon.svg at 180 by 180
    site/media/social.png            the link preview: the mark on navy, 1200 by 630
    art/nuclei-banner-2000x500.png   the banner: the mark and the word Nuclei on navy, 2000 by 500
    art/nuclei-banner-1280x640.png   the banner at 1280 by 640 (a repository's social preview)

and it writes the header mark inline into the landing page's source (site/src/index.html), in
heavier lines for its size, its arrows grouped for the animation in site/motion.css; the site's
build (site/build_pages.py) copies it from there into every page with the bar.
"""

import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The word of the banners is the site's wordmark: Geist SemiBold (art/fonts, under its OFL),
# tracked -0.04 em, in the site's ink, --color-ink: oklch(96% 0.008 230).
FONT = ROOT / "art/fonts/Geist-SemiBold.ttf"
INK = "#edf3f6"
TRACKING = -0.04

# The line width of the original drawing, on the 1024 grid.
STROKE = 11
NAVY = ("#0d0f2b", "#0e0a22")

# Paint servers. Each is a linear gradient in the coordinates of the unit it colours, as drawn
# pointing up (the axial unit) or up and left (the diagonal unit), so a rotated copy carries its
# colours round with it. "core" is the exception: the diamonds are drawn once, unrotated.
GRADIENTS = [
    ("core", (600, 400, 424, 624), [(0, "#56f2e0"), (1, "#24a6f2")]),
    ("ax-shaft", (512, 395, 512, 232), [(0, "#4ef0e2"), (1, "#4684dd")]),
    ("ax-head", (454, 0, 570, 0), [(0, "#2ee3ee"), (0.42, "#33c3ef"), (0.58, "#5856c8"), (1, "#6a3bba")]),
    ("chev-a", (0, 190, 0, 294), [(0, "#42e9e8"), (1, "#4ef2de")]),
    ("chev-b", (0, 190, 0, 294), [(0, "#1fa3f5"), (1, "#2cb2ec")]),
    ("dg-shaft-a", (453.5, 453.5, 260, 260), [(0, "#4cf4e6"), (1, "#22e8e8")]),
    ("dg-shaft-b", (453.5, 453.5, 260, 260), [(0, "#3de8f4"), (1, "#5262cc")]),
    ("dg-head-a", (314, 398, 398, 314), [(0, "#36ebe3"), (0.5, "#2bc6ec"), (1, "#4a7fd8")]),
    ("dg-head-b", (314, 398, 398, 314), [(0, "#3a98e2"), (0.5, "#4f6ad2"), (1, "#6240bb")]),
    ("bracket", (367, 226, 226, 367), [(0, "#6243b6"), (0.5, "#5a44bb"), (1, "#4290d6")]),
]

# The units, as drawn for the arm pointing up (axial) and up and left (diagonal).
#   ax-shaft:   from the outer diamond's top corner to the arrow's tip.
#   ax-head:    the arrowhead; its inner sides run down to the gap around the outer diamond,
#               where the clip cuts them parallel to its edges.
#   ax-chev:    the chevron beyond the tip.
#   dg-shaft:   from the middle of the outer diamond's edge to the tip.
#   dg-head:    the square arrowhead: its sides turn back along the shaft, one ending at the
#               gap, the other turning into the hook.
#   dg-bracket: the corner bracket beyond the tip.
PATHS = [
    ("ax-shaft", "M512 395V232", False),
    ("ax-head", "M482 400V318L454 290L512 232L570 290L542 318V400", True),
    ("ax-chev", "M408 294L512 190L616 294", False),
    ("dg-shaft", "M453.5 453.5L260 260", False),
    ("dg-head", "M458.25 416.75L367 325.5V260H260V367H325.5L408.5 450H348", False),
    ("dg-bracket", "M367 226H226V367", False),
]

DIAMONDS = ["M512 448L576 512L512 576L448 512Z", "M512 395L629 512L512 629L395 512Z"]

# The paint of each part, per rotation. The chevrons up and right are turquoise, down and left
# blue; the diagonal arrows up-left and down-right lean turquoise, the other two violet.
ROTATIONS = [
    (0, "chev-a", "a"),
    (90, "chev-a", "b"),
    (180, "chev-b", "a"),
    (270, "chev-b", "b"),
]

# The mark's extent on the grid, chevron tips included: 182 to 842 on both axes.
TIGHT = (176, 176, 672, 672)


def num(value):
    return f"{value:g}"


def drawing(stroke, transform=None, prefix="", moving=False):
    """The defs and the mark, as lines of SVG.

    `prefix` starts every id, so the mark can sit inline in a page beside other ids. `moving`
    gives the header mark the classes its animation moves: each arrow's outer part (the head
    with its chevron or its bracket) as one group, and each shaft on its own, which stretches.
    """
    ids = {name: f"{prefix}{name}" for name, _, _ in GRADIENTS}
    out = ["  <defs>"]
    for name, (x1, y1, x2, y2), stops in GRADIENTS:
        out.append(
            f'    <linearGradient id="{ids[name]}" gradientUnits="userSpaceOnUse" '
            f'x1="{num(x1)}" y1="{num(y1)}" x2="{num(x2)}" y2="{num(y2)}">'
        )
        out += [f'      <stop offset="{num(o)}" stop-color="{c}"/>' for o, c in stops]
        out.append("    </linearGradient>")
    out.append(f'    <clipPath id="{prefix}gap">')
    out.append('      <path clip-rule="evenodd" d="M0 0H1024V1024H0ZM512 363L661 512L512 661L363 512Z"/>')
    out.append("    </clipPath>")
    for name, d, clipped in PATHS:
        clip = f' clip-path="url(#{prefix}gap)"' if clipped else ""
        out.append(f'    <path id="{prefix}p-{name}"{clip} d="{d}"/>')
    out.append("  </defs>")
    group = f'  <g fill="none" stroke-width="{num(stroke)}" stroke-linejoin="miter" stroke-miterlimit="4"'
    out.append(group + (f' transform="{transform}">' if transform else ">"))
    out += [f'    <path stroke="url(#{ids["core"]})" d="{d}"/>' for d in DIAMONDS]

    def use(part, paint, cls=""):
        cls = f' class="{prefix}{cls}"' if moving and cls else ""
        return f'<use{cls} href="#{prefix}p-{part}" stroke="url(#{ids[paint]})"/>'

    for angle, chevron, kind in ROTATIONS:
        out.append(f'    <g transform="rotate({angle} 512 512)">' if angle else "    <g>")
        out.append("      " + use("ax-shaft", "ax-shaft", "ax-shaft"))
        outer = [use("ax-head", "ax-head"), use("ax-chev", chevron)]
        out += [f'      <g class="{prefix}ax-out">' + "".join(outer) + "</g>"] if moving else ["      " + u for u in outer]
        out.append("      " + use("dg-shaft", f"dg-shaft-{kind}", "dg-shaft"))
        outer = [use("dg-head", f"dg-head-{kind}"), use("dg-bracket", "bracket")]
        out += [f'      <g class="{prefix}dg-out">' + "".join(outer) + "</g>"] if moving else ["      " + u for u in outer]
        out.append("    </g>")
    out.append("  </g>")
    return out


def header_mark():
    """The header mark, inline in every page so its arrows can move: heavier lines for its few
    dozen pixels, ids that start with nm-, and hidden from assistive technology (the link's
    text names it)."""
    lines = drawing(16, prefix="nm-", moving=True)
    x, y, w, h = TIGHT
    return (f'<svg class="nav__mark-svg" viewBox="{x} {y} {w} {h}" width="44" height="44" '
            f'aria-hidden="true" focusable="false">' + "".join(line.strip() for line in lines) + "</svg>")


def put_header_mark(mark):
    """Writes the header mark into the landing page's source, where the site's build takes it for
    every page with the bar."""
    pages = [ROOT / "site/src/index.html"]
    pattern = re.compile(r'(<span class="nav__logo">).*?(</span>)', re.S)
    for page in pages:
        text = page.read_text()
        new, count = pattern.subn(lambda m: m.group(1) + mark + m.group(2), text)
        if count != 1:
            sys.exit(f"{page.relative_to(ROOT)}: expected one header mark, found {count}")
        page.write_text(new)
    return pages


def svg(view_box, width, height, stroke, navy=None, transform=None):
    """A whole SVG file. `navy` is the rectangle (x, y, w, h) the navy background fills."""
    x, y, w, h = view_box
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{num(x)} {num(y)} {num(w)} {num(h)}" '
        f'width="{num(width)}" height="{num(height)}">'
    ]
    if navy:
        bx, by, bw, bh = navy
        cx, cy = bx + bw / 2, by + bh / 2
        radius = (bw * bw + bh * bh) ** 0.5 / 2
        out.append("  <defs>")
        out.append(
            f'    <radialGradient id="navy" gradientUnits="userSpaceOnUse" '
            f'cx="{num(cx)}" cy="{num(cy)}" r="{num(radius)}">'
        )
        out.append(f'      <stop offset="0" stop-color="{NAVY[0]}"/>')
        out.append(f'      <stop offset="1" stop-color="{NAVY[1]}"/>')
        out.append("    </radialGradient>")
        out.append("  </defs>")
        out.append(f'  <rect x="{num(bx)}" y="{num(by)}" width="{num(bw)}" height="{num(bh)}" fill="url(#navy)"/>')
    out += drawing(stroke, transform)
    out.append("</svg>")
    return "\n".join(out) + "\n"


def bare_png(path):
    """Rewrites a PNG with only the chunks that make the image (IHDR, PLTE, tRNS, IDAT, IEND)."""
    data = path.read_bytes()
    signature = b"\x89PNG\r\n\x1a\n"
    if not data.startswith(signature):
        raise SystemExit(f"{path}: not a PNG file")
    keep = {b"IHDR", b"PLTE", b"tRNS", b"IDAT", b"IEND"}
    out, pos = [signature], len(signature)
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        end = pos + 12 + length
        if data[pos + 4 : pos + 8] in keep:
            out.append(data[pos:end])
        pos = end
    path.write_bytes(b"".join(out))


def render(source, target, width, height, env=None):
    subprocess.run(
        ["rsvg-convert", "--width", str(width), "--height", str(height), "--output", str(target), str(source)],
        check=True,
        env=env,
    )
    bare_png(target)


def banner(width, height, mark_share):
    """A banner of `width` by `height`: the mark, `mark_share` of the height tall, and the word
    Nuclei beside it in the site's wordmark, both centred as one on navy. The word's width and
    cap height, for the centring, are measured with Pillow from the same font file."""
    from PIL import ImageFont

    word = "Nuclei"
    mark = height * mark_share
    size = mark * 0.6
    font = ImageFont.truetype(str(FONT), round(size))
    left, _, right, _ = font.getbbox(word, anchor="ls")
    cap = -font.getbbox("N", anchor="ls")[1]
    tracking = TRACKING * size
    text_width = right - left + tracking * (len(word) - 1)
    gap = mark * 0.2
    x0 = (width - (mark + gap + text_width)) / 2
    scale = mark / TIGHT[2]
    out = svg(
        (0, 0, width, height),
        width,
        height,
        STROKE * 1.4,
        navy=(0, 0, width, height),
        transform=f"translate({num(x0 + mark / 2)} {num(height / 2)}) scale({scale:.6f}) translate(-512 -512)",
    ).replace(
        "</svg>",
        f'  <text x="{num(x0 + mark + gap - left)}" y="{num(height / 2 + cap / 2)}" font-family="Geist" '
        f'font-weight="600" font-size="{num(size)}" letter-spacing="{num(tracking)}" fill="{INK}">{word}</text>\n</svg>',
    )
    return out


def font_env(scratch):
    """The environment rsvg-convert renders the banners in: fontconfig that finds Geist in
    art/fonts, the system's fonts kept."""
    conf = Path(scratch) / "fonts.conf"
    conf.write_text(
        '<?xml version="1.0"?>\n<fontconfig>\n'
        f"  <dir>{FONT.parent}</dir>\n"
        '  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>\n'
        f"  <cachedir>{scratch}/fontcache</cachedir>\n</fontconfig>\n"
    )
    return {**os.environ, "FONTCONFIG_FILE": str(conf)}


def main():
    if shutil.which("rsvg-convert") is None:
        sys.exit("rsvg-convert is needed to render the PNG files: install librsvg and run this again")

    social_scale = 430 / TIGHT[2]
    svgs = {
        "art/nuclei-logo.svg": svg(TIGHT, 672, 672, STROKE),
        "art/nuclei-logo-dark.svg": svg((0, 0, 1024, 1024), 1024, 1024, STROKE, navy=(0, 0, 1024, 1024)),
        "site/favicon.svg": svg((136, 136, 752, 752), 752, 752, 20, navy=(136, 136, 752, 752)),
    }
    for name, text in svgs.items():
        (ROOT / name).write_text(text)

    with tempfile.TemporaryDirectory() as scratch:
        social = Path(scratch) / "social.svg"
        social.write_text(
            svg(
                (0, 0, 1200, 630),
                1200,
                630,
                STROKE * 1.4,
                navy=(0, 0, 1200, 630),
                transform=f"translate(600 315) scale({social_scale:.6f}) translate(-512 -512)",
            )
        )
        render(social, ROOT / "site/media/social.png", 1200, 630)
        env = font_env(scratch)
        for width, height, share in [(2000, 500, 0.6), (1280, 640, 0.42)]:
            source = Path(scratch) / f"banner-{width}x{height}.svg"
            source.write_text(banner(width, height, share))
            render(source, ROOT / f"art/nuclei-banner-{width}x{height}.png", width, height, env)

    renders = [
        ("art/nuclei-logo.svg", "art/nuclei-logo-512.png", 512, 512),
        ("art/nuclei-logo.svg", "art/nuclei-logo-1024.png", 1024, 1024),
        ("art/nuclei-logo-dark.svg", "art/nuclei-logo-square.png", 1024, 1024),
        ("art/nuclei-logo-dark.svg", "art/nuclei-logo-square-512.png", 512, 512),
        ("site/favicon.svg", "site/favicon-32.png", 32, 32),
        ("site/favicon.svg", "site/apple-touch-icon.png", 180, 180),
    ]
    for source, target, width, height in renders:
        render(ROOT / source, ROOT / target, width, height)
    pages = put_header_mark(header_mark())
    banners = ["art/nuclei-banner-2000x500.png", "art/nuclei-banner-1280x640.png"]
    for name in [*svgs, "site/media/social.png", *banners, *(target for _, target, _, _ in renders)]:
        print(name)
    print(f"the header mark, inline in {pages[0].relative_to(ROOT)}")


if __name__ == "__main__":
    main()
