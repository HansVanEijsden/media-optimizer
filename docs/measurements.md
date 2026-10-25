# Measurements

Every performance and size number in this repository comes from the measurements
below. Nothing is estimated from a specification or copied from a vendor
benchmark. This document records the method, the tables and the confidence
limits.

## Method

- **Corpus.** 40 real production photographs, stored at quality 50 to 99, plus a
  220-file pilot tree. The photographs cover the quality bands the decision
  rules care about. The pilot tree is a real media tree with a mixed content
  mix.
- **Reference encoder.** mozjpeg-equivalent settings: libvips 8.18.7 built
  against mozjpeg 4.1.5, with `--quant-table 3`, `--optimize-coding`,
  `--interlace`, `--trellis-quant`, `--overshoot-deringing` and
  `--optimize-scans`.
- **Metric.** SSIMULACRA2, original against encoded output, 0 to 100. The tool
  keeps the measured score in the manifest for every lossy result it accepts.
- **Re-encode comparisons.** Each source was encoded at q85 and the size change
  was grouped by the source's stored quality band. The stored quality is the
  tool's own header-based estimate, not a value from a file tag.
- **Lossless comparisons.** `jpegtran -optimize -progressive` was applied to 28
  files at or below stored q85, and the size change per file was recorded. A
  result that grew was a rejection, exactly as the tool treats it.
- **Density comparisons.** The same source was encoded with mozjpeg `cjpeg` and
  with libvips, and the byte sizes were compared at equal SSIMULACRA2.
- **Throughput.** Encoding a 2048x1366 photo, once through the vips CLI and once
  in process.
- **Resize comparisons.** Downscale to 2048 on the longest side, then mozjpeg
  q85. Every kernel is reported relative to lanczos3.
- **PNG comparisons.** Lossless `oxipng`, `pngquant` on opaque PNGs, and
  `oxipng` on the palette output.
- **Pilot.** A full `mediaopt run` on the 220-file tree, then two more runs.

## JPEG re-encode by stored quality

| Source stored quality | Result at q85 |
| --- | --- |
| q96 to q100 | 71% smaller |
| q88 to q90 | 15.5% to 17.5% smaller |
| around q82 | about 0% |

This table is the reason `jpeg.reencode_min_quality` defaults to 86. Below that
line a lossy re-encode buys almost nothing and costs a generation of quality.
Above it, the gain is large enough to justify the encode.

## Lossless JPEG pass

| Measure | Result |
| --- | --- |
| Files measured | 28, all at stored quality <= q85 |
| Average size change | 4.78% smaller |
| Best file | 8.2% smaller |
| Files that grew | 3, by 0.3% to 1.6% |
| Action on growth | rejected, original kept |

The three growing files are the reason the lossless pass has a gain gate at all.
Progressive and optimal Huffman coding are not guaranteed to win on every file.

## Encoder density and throughput

| Comparison | Result |
| --- | --- |
| libvips 8.18.7 + mozjpeg 4.1.5 vs mozjpeg `cjpeg`, `--quant-table 3` | within 0.1% at identical SSIMULACRA2 |
| the same comparison with libvips's default quantisation table | 2.9% to 5.2% larger |
| libvips CLI throughput | 3.54 images/s |
| libvips in-process throughput | 4.14 images/s |
| Test image | 2048x1366 photo |

The CLI path is slower because every encode crosses a process boundary. The
default table result is why `engine.rs` hardcodes table 3.

## mozjpeg against libjpeg-turbo

| Comparison at SSIMULACRA2 78 | Result |
| --- | --- |
| mozjpeg vs libjpeg-turbo baseline | median 9.3% smaller |
| mozjpeg vs libjpeg-turbo progressive | 3.6% smaller |

Most of the mozjpeg advantage over a baseline libjpeg-turbo disappears when the
comparison is against libjpeg-turbo in progressive mode. A libvips built against
libjpeg-turbo still works, but it starts from a weaker position.

## Quality needed for a fixed perceptual floor

| Measure | Result |
| --- | --- |
| Quality range needed to reach SSIMULACRA2 78 | Q70 to Q92 across the corpus |

The spread is the reason `quality_mode = "target"` exists. A single fixed
quality cannot serve both ends of that range.

## Resize kernels

Downscale to 2048 on the longest side, then mozjpeg q85, relative to lanczos3.

| Kernel | Bytes vs lanczos3 | Note |
| --- | --- | --- |
| lanczos3 | reference | reference |
| Catmull-Rom (`cubic`) | -4.1% | 75% of the micro-contrast, no ringing; the default |
| Mitchell | -11.5% | visibly softer |
| box | +2.0% | not useful |
| DCT 1/2 pre-shrink | -11% | halves micro-contrast, rejected |
| lanczos3 + unsharp (sigma 0.5, amount 0.5) | +10.3% | halo tax, rejected |

## PNG

| Path | Result |
| --- | --- |
| `oxipng` lossless on already-optimised photo PNGs | 0.0% to 3% |
| `pngquant` palette on opaque PNGs | 53% to 68% |
| `oxipng` on top of the palette output | another 5% to 15% |

The lossless gain is small but never negative in the measured set, which is why
the lossless path is always safe to run. The palette gain is large and
content-dependent, which is why it has a quality floor and a gain gate.

## Pilot result

| Measure | Result |
| --- | --- |
| Files in the tree | 220 |
| Changed on the first run | 165 |
| Rejected for no gain | 55 |
| Freed | 19.15 MiB |
| Changed on the second run | 0 |
| Changed on the third run | 0 |

The second and third runs are the operational proof of idempotency: the
manifest, the COM marker and the re-encode threshold together leave nothing to
do.

## Operational observations

These are not benchmarks, but they shaped the design and are worth recording.

| Observation | Value |
| --- | --- |
| Files in one production tree carrying the legacy `optimised` EXIF comment | 99.4% |
| Of those, still at q95 or above | 20% |
| Files named `.png` in one production tree that were actually JPEG | 14.3% |

The first two rows are why the legacy comment is ignored as a "done" marker and
why the tool writes its own COM marker plus a manifest. The third row is why the
format is sniffed from magic bytes and why file names are never changed.

## Confidence notes

- The corpus is 40 photographs plus a 220-file pilot tree. It is a real
  production sample, not a random sample of all images on the internet. A
  corpus dominated by screenshots, line art or already-stripped images would
  produce different averages.
- The percentage bands are valid for the stored quality bands measured. No
  measurement was made outside those bands. The tables do not separate
  confident from approximate quality estimates; the decision rules add a margin
  for the latter instead.
- SSIMULACRA2 is a perceptual metric, not a human panel. The phrase
  "indistinguishable at 1:1" for a score of 90 is the published scale's
  description, not a claim from a viewing test in this project.
- Throughput was measured on one 2048x1366 photo. It is not measured under CPU
  contention, on network storage, on cold cache, with a high `jobs` value, or
  while another run is active.
- Not measured: HEIC, AVIF, JPEG XL, TIFF, BMP, animated GIF, CMYK input, lossy
  or lossless WebP re-encoding, 16-bit PNG, very large panoramas, print
  workflows, energy use, and human visual quality beyond SSIMULACRA2.
- The legacy marker and misnamed-PNG figures come from one production tree each.
  They are observations, not corpus-wide statistics.

## Reproducing

The 0.1.0 tree does not ship a benchmark harness. The procedure that produced
the tables was:

1. Install libvips built against mozjpeg, mozjpeg `jpegtran`, `pngquant`, the
   `oxipng` CLI and `ssimulacra2`.
2. Group the sources by stored quality. `mediaopt` itself can print the estimate
   per file at trace level:

```console
RUST_LOG=mediaopt=trace mediaopt plan 2>&1 | grep 'format=Jpeg'
```

3. Encode each source and measure it against the original:

```console
vips jpegsave in.jpg out.jpg --Q 85 --optimize-coding --interlace \
  --trellis-quant --overshoot-deringing --optimize-scans \
  --quant-table 3 --keep icc,exif
ssimulacra2 in.jpg out.jpg
```

4. For the lossless pass:

```console
jpegtran -copy all -optimize -progressive -outfile out.jpg in.jpg
```

5. For PNG:

```console
pngquant --quality 80-100 --speed 1 --force --strip --output out.png in.png
oxipng -o 2 out.png
```

6. Divide total bytes before by total bytes after per quality band. For
   throughput, encode the same 2048x1366 photo repeatedly and divide the wall
   clock by the number of encodes; run the same loop through the CLI and through
   a process that links libvips.
7. For the pilot, run `mediaopt run` three times over the same 220-file tree and
   compare the `changed=` field of the summary line.

The exact encoder build matters. Record `mediaopt tools` output with every
measurement, because a different libvips or mozjpeg build can change the byte
sizes at the same score.
