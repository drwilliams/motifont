# motifont

A command-line tool that builds **motif fonts** from PDF artwork or scanned images. A motif font
is a TrueType font whose characters are logos, monograms, icons or decorative designs instead
of letters. It runs on macOS and Linux.

Give it a design and it scales the design into the em square, sits it on the baseline, puts it in
the **A** and **a** characters and saves a `.ttf` font. You can also put a different design in
each character, and convert a whole folder of designs in one go.

## Build

```bash
cargo build --release
```

The binary is `target/release/motifont`. Copy it anywhere on your `PATH`.

## Usage

Convert one design. This writes `heart.ttf` with the design in A and a:

```bash
motifont build Heart.pdf
```

Convert a whole folder of PDFs (and/or TIFF/PNG scans), one font per file:

```bash
motifont batch ~/Designs/incoming -o ~/Designs/fonts
```

Put several designs into one font:

```bash
motifont build Aa=heart.pdf Bb=star.pdf Cc=leaf.pdf -o collection.ttf --family "Motif Collection"
```

Use a specific page of a multi-page PDF with `@PAGE`, for example `Aa=sheet.pdf@2`.

### Scanned artwork

Any `.tif/.tiff/.png/.jpg/.bmp/.gif` given in place of a PDF is traced into smooth
outlines automatically. A 600 dpi black-and-white scan works well:

```bash
motifont build Heart.tif
```

Tuning options: `--threshold 128` sets the ink cutoff (0-255), `--invert` handles white-on-black art,
`--despeckle 16` removes specks smaller than that many px², `--tolerance 0.5` sets how tightly
the trace follows the pixels, and `--corner-angle 50` sets how sharp a turn must be to stay a corner.

### Placement and font options

| option | default | meaning |
|---|---|---|
| `--width` | 1000 | max design width (font units; em = 1000) |
| `--height` | 1000 | max design height |
| `--bottom` | 0 | y of the design's bottom edge (0 = sits on baseline) |
| `--lsb` / `--rsb` | 0 | space left / right of the design |
| `--advance` | design width | fixed advance width |
| `--vendor` | `NONE` | 4-letter OS/2 vendor ID (set your own foundry/company code) |
| `--font-version` | `1.000` | font version string |

The design is always scaled **proportionally**, so it is never stretched.

## What it handles in PDFs

- Filled paths, with both non-zero and even-odd fill rules (compound paths and holes).
- Strokes, which are converted to outlines.
- White or no-ink shapes painted over the art, which are cut out of the design (knockouts).
- Overlapping shapes, which are merged into clean non-overlapping contours.
- Nested Form XObjects, and any transforms or scaling.

You get a warning, and the item is skipped, for live text (convert it to outlines first),
embedded raster images (trace a scan instead) and gradients. Clipping masks are ignored.

## Output

Fonts use 1000 units per em, ascender 750 / descender −250, Regular weight 400, and
`fsType` 0 (installable, so they can be embedded in PDFs with no restrictions). The family name
defaults to the lower-case file name. Fonts are written unhinted. That's fine for print,
macOS and web rendering.

## Code layout

- `src/pdf.rs` is a PDF content-stream interpreter that extracts the vector shapes.
- `src/trace.rs` is the bitmap tracer (pixel-edge tracing, corner detection, and Schneider curve fitting).
- `src/outline.rs` handles boolean merging, contour direction, fitting into the em, and cubic→quadratic conversion.
- `src/font.rs` writes the TrueType tables.
- `src/main.rs` is the CLI.

## License

MIT. See [LICENSE](LICENSE).
