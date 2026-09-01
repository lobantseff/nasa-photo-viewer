#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow>=10.0",
# ]
# ///

"""
Render the application icon.

The icon is generated rather than drawn by hand so it can be reproduced at any
size, and so a change to it is a readable diff rather than an opaque binary
blob.

The design is a Mars disc seen through a camera aperture: the subject, and the
act of photographing it. It is built from two shapes and two colours because
the smallest size it has to survive is 16x16, where anything finer turns to
mud.

Usage:
    ./generate_icon.py            # write assets/AppIcon.{png,icns,ico}
    ./generate_icon.py --check    # verify the committed icons are up to date
"""

from __future__ import annotations

import argparse
import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

# Rendered large and downsampled, which antialiases every edge at once.
MASTER = 1024
SUPERSAMPLE = 4

# The sizes macOS expects inside an .iconset, as (pixels, filename).
ICNS_SIZES = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
]

# A Windows .ico carries its own sizes; 256 is the largest the format stores.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

SPACE = (16, 18, 26)
SPACE_EDGE = (8, 9, 14)
MARS_LIT = (226, 122, 62)
MARS_MID = (182, 78, 40)
MARS_DARK = (104, 38, 22)
# Barely lighter than the backdrop: the aperture is drawn with its edges, not
# with fill, so that Mars stays the subject rather than a hole in a bright ring.
BLADE = (33, 37, 50)
BLADE_EDGE = (208, 214, 226)

APERTURE_BLADES = 6

# Fractions of the icon's width.
# Clear of the hexagon flats (opening * cos30 = 0.351), so the aperture edge
# reads as deliberate framing rather than something Mars is bursting through.
MARS_RADIUS = 0.30
APERTURE_OPENING = 0.405
# Past the corner of the rounded square, so the blades run to the edge instead
# of floating as a ring inside it.
APERTURE_OUTER = 0.78


def lerp(
    a: tuple[int, int, int], b: tuple[int, int, int], t: float
) -> tuple[int, int, int]:
    """Blend two colours."""
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))  # type: ignore[return-value]


def render(size: int) -> Image.Image:
    """Draw the icon at `size` pixels square."""
    s = size * SUPERSAMPLE
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    # Rounded-square backdrop. Recent macOS does not mask icons to its own
    # shape, so an icon has to supply one or it reads as a floating sticker.
    radius = s * 0.22
    draw.rounded_rectangle([0, 0, s - 1, s - 1], radius=radius, fill=SPACE)
    draw.rounded_rectangle(
        [0, 0, s - 1, s - 1], radius=radius, outline=SPACE_EDGE, width=max(1, s // 128)
    )

    # Aperture blades first, so Mars sits over them and reads as the subject
    # seen through the opening rather than a disc pasted on top.
    cx = cy = s / 2
    ring_outer = s * APERTURE_OUTER
    opening = s * APERTURE_OPENING
    blades = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    blade_draw = ImageDraw.Draw(blades)
    edge_width = max(1, round(s / 150))

    for i in range(APERTURE_BLADES):
        a0 = 2 * math.pi * i / APERTURE_BLADES - math.pi / 2
        a1 = 2 * math.pi * (i + 1) / APERTURE_BLADES - math.pi / 2
        p0 = (cx + opening * math.cos(a0), cy + opening * math.sin(a0))
        p1 = (cx + opening * math.cos(a1), cy + opening * math.sin(a1))
        p2 = (cx + ring_outer * math.cos(a1), cy + ring_outer * math.sin(a1))
        p3 = (cx + ring_outer * math.cos(a0), cy + ring_outer * math.sin(a0))
        blade_draw.polygon([p0, p1, p2, p3], fill=BLADE)
        # The chord across the opening is the blade's leading edge, and the
        # radial line is where it meets its neighbour. Together they are what
        # makes the shape read as an aperture at all.
        blade_draw.line([p0, p1], fill=BLADE_EDGE, width=edge_width)
        blade_draw.line(
            [p0, (cx + ring_outer * math.cos(a0), cy + ring_outer * math.sin(a0))],
            fill=(*BLADE_EDGE, 70),
            width=max(1, edge_width // 2),
        )

    # Clip to the rounded square so the blades fill the corners.
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, s - 1, s - 1], radius=radius, fill=255
    )
    blades.putalpha(
        Image.composite(blades.getchannel("A"), Image.new("L", (s, s), 0), mask)
    )
    img.alpha_composite(blades)

    # Mars, lit from the upper left, sitting inside the opening.
    r = s * MARS_RADIUS
    steps = 96
    planet = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    planet_draw = ImageDraw.Draw(planet)
    for i in range(steps, 0, -1):
        t = i / steps
        colour = lerp(MARS_LIT, MARS_DARK, (1 - t) ** 1.4)
        rr = r * t
        # Drift the highlight up and left as the circles shrink.
        ox = cx - r * 0.20 * (1 - t)
        oy = cy - r * 0.20 * (1 - t)
        planet_draw.ellipse([ox - rr, oy - rr, ox + rr, oy + rr], fill=colour)

    # A few darker patches so the disc reads as a planet rather than a dot.
    # They disappear below about 32px, which is the intent: no clutter when the
    # icon is small.
    for fx, fy, fr in [
        (-0.30, -0.26, 0.22),
        (0.28, 0.12, 0.28),
        (-0.08, 0.44, 0.17),
        (0.42, -0.34, 0.14),
    ]:
        px, py, pr = cx + r * fx, cy + r * fy, r * fr
        patch = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        ImageDraw.Draw(patch).ellipse(
            [px - pr, py - pr, px + pr, py + pr], fill=(*MARS_DARK, 120)
        )
        patch = patch.filter(ImageFilter.GaussianBlur(pr * 0.4))
        planet.alpha_composite(patch)

    # Keep the patches inside the disc.
    disc = Image.new("L", (s, s), 0)
    ImageDraw.Draw(disc).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    planet.putalpha(
        Image.composite(planet.getchannel("A"), Image.new("L", (s, s), 0), disc)
    )
    img.alpha_composite(planet)

    return img.resize((size, size), Image.LANCZOS)


def write_png(path: Path) -> None:
    render(MASTER).save(path, "PNG")
    print(f"  {path.name}")


def write_ico(path: Path) -> None:
    # Each size is drawn rather than downscaled from one bitmap, so the small
    # ones keep their contrast. Pillow scales from the base image, so that has
    # to be the largest; the per-size renders are supplied to replace its
    # scaled-down versions.
    images = {n: render(n) for n in ICO_SIZES}
    largest = images[max(ICO_SIZES)]
    largest.save(
        path,
        "ICO",
        sizes=[(n, n) for n in ICO_SIZES],
        append_images=[images[n] for n in ICO_SIZES if n != max(ICO_SIZES)],
    )

    stored = sorted(Image.open(path).info.get("sizes", []))
    if stored != sorted((n, n) for n in ICO_SIZES):
        print(f"error: {path.name} stored only {stored}", file=sys.stderr)
        sys.exit(1)
    print(f"  {path.name} ({len(ICO_SIZES)} sizes)")


def write_icns(path: Path) -> None:
    if not shutil.which("iconutil"):
        print("  AppIcon.icns skipped: iconutil exists only on macOS")
        return

    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for size, name in ICNS_SIZES:
            render(size).save(iconset / name, "PNG")

        result = subprocess.run(
            ["iconutil", "--convert", "icns", str(iconset), "--output", str(path)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            print(f"error: iconutil failed:\n{result.stderr}", file=sys.stderr)
            sys.exit(1)
    print(f"  {path.name}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Render the application icon.")
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the committed icon differs from a freshly rendered one",
    )
    args = parser.parse_args()

    assets = Path(__file__).resolve().parent

    if args.check:
        actual_path = assets / "AppIcon.png"
        if not actual_path.exists():
            print(
                "error: AppIcon.png is missing; run ./generate_icon.py", file=sys.stderr
            )
            sys.exit(1)
        expected = render(MASTER)
        actual = Image.open(actual_path).convert("RGBA")
        if actual.tobytes() != expected.tobytes():
            print(
                "error: AppIcon.png differs from the generator; run ./generate_icon.py",
                file=sys.stderr,
            )
            sys.exit(1)
        print("icons are up to date")
        return

    print("Rendering icons into assets/")
    write_png(assets / "AppIcon.png")
    write_ico(assets / "AppIcon.ico")
    write_icns(assets / "AppIcon.icns")


if __name__ == "__main__":
    main()
