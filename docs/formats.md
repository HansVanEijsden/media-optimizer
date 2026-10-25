# Formats

This document explains the perceptual scale the tool optimises against and the
per-format rules that follow from the measurements.

## The SSIMULACRA2 scale

SSIMULACRA2 is a full-reference perceptual metric. The tool compares the
original file with the encoded temporary file and keeps the score. Higher is
better; 100 means identical.

| Score | Meaning |
| --- | --- |
| 30 | low quality |
| 50 | medium quality |
| 70 | high quality, the level of mozjpeg `-quality 70` |
| 90 | very high quality, indistinguishable at 1:1 |
| 100 | identical |

The default floor is 75, described in the config as high quality and close to a
good mozjpeg q85. The floor is validated to stay between 60 and 100, so a typo
cannot disable the quality guard silently.

## What the stored quality estimate means

The stored quality is not read from a tag. The probe parses the JPEG header,
extracts the luminance quantisation table, and finds the IJG Annex K scale
factor whose scaled table has the smallest total deviation from the stored one.
The matching factor is reported as the stored quality, and the deviation is
used as a confidence signal.

- If the deviation is small, the table looks like an IJG-scaled table and the
  estimate is confident.
- If no scale factor reproduces the table, the file was encoded with a custom
  quantisation table. The estimate is then a rough guide and the row is stored
  with `quality_confident = 0`.

Custom tables need the margin because a wrong estimate must never trigger a
lossy re-encode of a file that is in fact already lean. When the estimate is not
confident, the re-encode threshold is raised by 3. A file that reads just above
the threshold but carries a custom table is treated as below it, so it gets the
lossless pass instead. The gain gate and the SSIMULACRA2 floor are the second
line of defence if such a file still reaches the encoder.

The estimate describes the encoder setting that produced the source, not how
good it looks. A good-looking source from a weak encoder can score worse than a
lower-quality source from mozjpeg. That is why the re-encode decision uses the
estimate, but the result is accepted only on the measured SSIMULACRA2 score.

## JPEG: when the tool re-encodes and when it does not

Re-encoding an already-lean JPEG costs a generation of quality for almost no
bytes. The measurements made the cutoff obvious:

| Stored quality of the source | Re-encoding at q85 |
| --- | --- |
| q96 to q100 | 71% smaller |
| q88 to q90 | 15.5% to 17.5% smaller |
| around q82 | about 0% |

The default `reencode_min_quality` is 86. Everything at or above it is
re-encoded; everything below it gets the lossless pass. This has a useful side
effect: in `fixed` mode the tool's own output is q85, below 86, so ordinary
output is not a re-encode candidate. `target` mode settles at or below 85 for
most files, but a file that needs a higher quality to hold the floor can stay
above it; the COM marker and the manifest cover those files.

In `target` mode the encoder does not use one global quality. The quality needed
to reach SSIMULACRA2 78 varies from Q70 to Q92 across a real corpus, so a fixed
quality is either wasteful or risky. The tool starts near the stored quality,
encodes, measures, and walks the quality ladder until the floor is missed,
keeping the lowest quality that still met it. A source that needs a quality near
the top of that range is encoded much higher than one that holds the floor near
the bottom.

The lossless pass is `jpegtran -copy all -optimize -progressive`. Pixels are
untouched. On 28 files at or below stored q85 it averaged 4.78%, with a best
file at 8.2%. Three files grew by 0.3% to 1.6% and were rejected: with a
lossless codec there is no reason to keep a larger file.

## Downscaling

When the longest side exceeds `jpeg.max_dimension` (default 2048), the file is
downscaled and re-encoded at `jpeg.fixed_quality`. A quality search is not used
on this path because the output cannot be compared to the original pixel for
pixel; the scale itself is the change. A downscale frees more than any re-encode
of the full-size image, so it is always taken when the source is oversized,
including for sources that are already lean.

The default kernel is `cubic` in libvips, which is Catmull-Rom. Relative to
lanczos3, measured at the same final quality:

| Kernel | Bytes vs lanczos3 | Note |
| --- | --- | --- |
| lanczos3 | reference | reference |
| Catmull-Rom (`cubic`) | -4.1% | 75% of the micro-contrast, no ringing; the default |
| Mitchell | -11.5% | visibly softer |
| box | +2.0% | not useful here |
| DCT 1/2 pre-shrink | -11% | halves micro-contrast, rejected |
| lanczos3 + unsharp (sigma 0.5, amount 0.5) | +10.3% | halo tax, rejected |

Catmull-Rom wins because it removes bytes without the halo tax of the sharpened
lanczos3 variant. Mitchell saves more but is visibly softer, and the pre-shrink
halves micro-contrast, which is exactly the detail a photograph needs.

## Chroma subsampling

`jpeg.subsample` defaults to `auto`, which follows the source:

- a 4:4:4 source stays 4:4:4;
- a 4:2:0 or 4:2:2 source is re-encoded as 4:2:0.

4:4:4 sources are usually graphics, screenshots or images with coloured text.
Converting them to 4:2:0 smears coloured edges, and those files are often small
enough that the bytes do not matter. `on` forces 4:2:0 for a photograph-only
tree; `off` forces 4:4:4 for a graphics-only tree.

Subsampling only applies to re-encoded JPEGs. The lossless pass never changes
the sampling factors or the pixels.

## Metadata: ICC, EXIF, XMP, IPTC

Colour is never converted, and ICC profiles are kept by default:

- **JPEG re-encode.** `jpeg.keep` defaults to `["icc", "exif"]`, passed to
  libvips as `keep=icc,exif`. XMP and IPTC are not in the default list and are
  dropped on this path. An empty list becomes `keep=none` and strips ICC and
  EXIF too, so do not empty it on production media.
- **JPEG lossless pass.** `jpegtran -copy all` copies every metadata segment
  verbatim. Nothing is converted and nothing is stripped.
- **PNG lossless.** `oxipng` runs with stripping disabled, so every chunk
  survives.
- **PNG palette.** The route is `pngquant --strip`, which drops ancillary
  chunks. A PNG with an ICC profile is therefore never palette-quantised; it is
  routed to the lossless path so the profile cannot be lost. A PNG with alpha or
  an existing palette is routed to lossless as well. A palette PNG can still
  lose textual metadata or EXIF chunks that pngquant strips; that is the one
  place where ancillary metadata is not preserved.

EXIF orientation is parsed and recorded but never used to rotate pixels. The
metadata itself is preserved, so viewers keep interpreting the file the same
way. Widths and heights are the stored pixel dimensions, not the displayed
orientation.

## CMYK JPEG

A JPEG with four components is skipped with reason `cmyk`. Browsers and the
encoder disagree about the meaning of CMYK and YCCK data: an encoder that
guesses wrong turns the image magenta or inverts the tones. The tool has no
colour-managed path in v1, so it leaves those files alone.

## Palette PNG trade-offs

Palette quantisation is the biggest single win in the tool and the one that
needs the most care:

| Path | Measured gain |
| --- | --- |
| `oxipng` lossless on already-optimised photo PNGs | 0.0% to 3% |
| `pngquant` palette on opaque PNGs | 53% to 68% |
| `oxipng` on top of the palette output | another 5% to 15% |

The first row is why the lossless path exists anyway: it is free and never
worse. The second row is why the palette path exists. The risk is banding: a
palette is enough for screenshots and simple photographs, but a smooth gradient
can show steps. pngquant is asked for a quality floor (`png.quality_floor`,
default 80) and returns a refusal status (99 for quality, 98 for too few
colours) when it cannot meet it; the tool then falls back to lossless. The
palette result must also be at least `png.min_gain_percent` smaller, otherwise
the original stays.

PNGs with alpha, an existing palette or an ICC profile are never palette
candidates. The tool uses `pngquant --quality <floor>-100 --speed 1`, so
pngquant itself refuses to write a result it considers worse than the floor.

## Metric comparison

Several metrics are in common use. The differences that matter for an
unattended optimiser are range, direction and usable resolution.

| Metric | Range and direction | What it models | Used here |
| --- | --- | --- | --- |
| SSIM | 0 to 1, higher is better | local luminance, contrast and structure correlation | no |
| MS-SSIM | 0 to 1, higher is better | SSIM across multiple scales | no |
| Butteraugli | distance, higher is worse | perceptual difference with a pooling model | no |
| SmallFry | not used here | perceptual metric used by jpeg-archive; the project is unmaintained and the metric is broken on recent builds | no |
| SSIMULACRA2 | 0 to 100, higher is better | full-reference perceptual distance | yes |

The SSIM target offered by jpeg-recompress has almost no usable resolution. On
this corpus, SSIM 0.9999 still corresponds to SSIMULACRA2 74, and SSIM 0.995 to
SSIMULACRA2 43. The whole meaningful range of the metric is squeezed into the
last few thousandths, so a target such as 0.995 looks precise while it actually
allows visibly degraded output. That is also why the tool does not offer an
SSIM target and why jpeg-archive/SmallFry is not a drop-in replacement: it is
unmaintained, and its metric is broken on recent builds.

SSIMULACRA2 is measured with the external `ssimulacra2` binary, original against
temporary output, for every lossy re-encode that is not a downscale; a resized
output has no pixel-aligned comparison, so it uses the configured quality
without a score. Without the binary, `target` degrades to a single encode at
`fixed_quality`; the config validation does not require the binary because the
tool remains useful without it, but the quality search does not run.
