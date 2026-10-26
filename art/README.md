# The Nuclei logo

The mark: a diamond inside a larger diamond, eight arrows pointing out along the axes and the
diagonals, and four corner brackets, in lines of one width coloured from turquoise and cyan
through blue to violet.

| File | What it is |
| --- | --- |
| `nuclei-logo.svg` | The mark on a transparent background, cropped to it. |
| `nuclei-logo-dark.svg` | The mark on its navy square, 1024 by 1024. |
| `nuclei-logo-512.png`, `nuclei-logo-1024.png` | `nuclei-logo.svg` rendered at 512 and 1024 pixels square. |
| `nuclei-logo-square.png` | `nuclei-logo-dark.svg` rendered at 1024 by 1024. |
| `nuclei-logo-square-512.png` | `nuclei-logo-dark.svg` rendered at 512 by 512. |
| `build.py` | Draws the mark and writes every file above, and the site's copies. |

The site uses the same drawing with heavier lines where it is shown small: the header mark,
which `build.py` writes inline into every page (`site/index.html`, `site/learn/*.html`) so its
arrows can breathe, `site/favicon.svg` with `site/favicon-32.png` and
`site/apple-touch-icon.png`, and `site/media/social.png` (the link preview).

Change the drawing in `build.py`, never the output files, and rebuild everything with:

```sh
python3 art/build.py
```

It needs `rsvg-convert` (librsvg) for the PNG files. The SVG files carry no metadata, and the
PNG files keep only their image chunks.
