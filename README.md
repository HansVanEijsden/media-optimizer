# mediaopt

`mediaopt` is an idempotent, quality-targeted optimiser for large, live image
trees. One TOML file lists the directories it may touch; the tool probes each
image from its header, decides exactly one action per file, encodes into a
temporary file next to the original, verifies that the result decodes at the
expected size, and only then replaces the original atomically. JPEG quality is
searched per file against an SSIMULACRA2 floor. Files that are already lean get
a lossless `jpegtran` pass instead of another generation of lossy encoding.

It is written for trees that a web server is serving while the tool works. It
runs at low priority, holds a single-instance lock, respects per-run time and
byte budgets, preserves ownership, mode and timestamps, and never renames or
converts a file. A SQLite manifest plus a COM marker in every JPEG it writes
makes the tool idempotent. In the 220-file pilot, the first run changed 165
files, and the second and third runs changed 0.

## What it does

- Re-encodes JPEGs whose stored quality is at or above
  `jpeg.reencode_min_quality` (default 86), searching the lowest quality that
  still meets the SSIMULACRA2 floor (default 75).
- Downscales JPEGs whose longest side exceeds `jpeg.max_dimension` (default
  2048) and re-encodes them at `jpeg.fixed_quality` (default 85).
- Runs `jpegtran -optimize -progressive -copy all` on JPEGs below the re-encode
  threshold. Measured on 28 such files: 4.78% average, best 8.2%.
- Palette-quantises opaque PNGs with `pngquant`, then recompresses the palette
  output with `oxipng`. Measured 53-68% on opaque PNGs.
- Recompresses all other PNGs losslessly with `oxipng` (in process).
- Records every decision in a manifest so the next run can skip the file without
  reading it again.
- Keeps a per-run JSON report and one-line summary suitable for journald.

## What it does not do

- No format conversion and no renaming. The format is sniffed from magic bytes,
  not from the file extension.
- No lossy WebP. Lossy WebP is skipped. Lossless WebP is recognised but
  re-packing it is not implemented in 0.1.0.
- No GIF, AVIF, HEIC, TIFF, BMP or JPEG XL. These are skipped as out of scope.
- No CMYK/YCCK JPEGs: browsers and the encoder disagree about the colours.
- No blanket metadata stripping. ICC and EXIF are kept; XMP and IPTC are dropped
  on a JPEG re-encode; the lossless pass copies every metadata segment verbatim.
- No network access at all. It does not fetch, upload or phone home.

## Decision table

| Source | Condition | Action |
| --- | --- | --- |
| JPEG | longest side > `jpeg.max_dimension` | downscale, re-encode at `fixed_quality` |
| JPEG | stored q >= `reencode_min_quality` (86 by default, +3 if the quality estimate is not confident) | re-encode, search the floor |
| JPEG | stored q below the re-encode threshold | `jpegtran` lossless pass |
| JPEG | 4 components (CMYK/YCCK) | skip |
| PNG | opaque, no palette, no ICC | `pngquant`, then `oxipng` |
| PNG | alpha, palette or ICC profile | `oxipng` lossless only |
| WebP | lossy | skip |
| WebP | lossless | not implemented in 0.1.0 |
| GIF, AVIF, HEIC, TIFF, BMP, JXL | any | skip |
| all | smaller than `limits.min_file_size`, or younger than `limits.min_file_age` | skip, not recorded |

Roots configured with `policy = "lossless_only"` never re-encode, resize or
palette-quantise anything.

## Install

### Container

The published container image carries a libvips built against mozjpeg 4.1.5,
plus `jpegtran`, `pngquant` and `ssimulacra2`, so it is the reliable way to get
mozjpeg density. Mount the configuration, the media tree and the state
directory:

```console
docker run --rm \
  -v /etc/mediaopt:/etc/mediaopt:ro \
  -v /mnt/media:/media \
  -v /srv/mediaopt:/var/lib/mediaopt \
  ghcr.io/hansvaneijsden/media-optimizer:0.1.0 \
  run
```

The state volume must be persistent: it holds the manifest and the reports.

### Native

Rust 1.85 or newer is needed to build.

```console
cargo build --release
install -m 0755 target/release/mediaopt /usr/local/bin/mediaopt
```

Requirements on `PATH` (or set explicitly in `[engine]`):

| Binary | Needed for | Notes |
| --- | --- | --- |
| `vips` and `vipsheader` | every JPEG re-encode and every verification | must be built against mozjpeg for mozjpeg density |
| `jpegtran` | the lossless JPEG pass | mozjpeg's `jpegtran`; it only understands the single-dash `-version` form |
| `pngquant` | palette PNG | 0.1.0 always uses `--speed 1` |
| `ssimulacra2` | quality search in `target` mode | without it, `target` falls back to one encode at `fixed_quality` |

A Homebrew `libvips` is linked against libjpeg-turbo, not mozjpeg. It works, but
its output is larger: mozjpeg is a median 9.3% smaller than libjpeg-turbo at
SSIMULACRA2 78, although only 3.6% smaller than libjpeg-turbo in progressive
mode. A libvips built against mozjpeg 4.1.5 matched `cjpeg` density within 0.1%
at identical SSIMULACRA2, but only with `--quant-table 3`; the default table was
2.9-5.2% larger. The tool always passes table 3.

## Quickstart

```console
mkdir -p /var/lib/mediaopt /etc/mediaopt
$EDITOR /etc/mediaopt/config.toml
mediaopt tools
mediaopt plan
mediaopt run
```

`plan` is `run --dry-run`: it encodes into a temporary file and throws the
result away, so its per-file savings are measured rather than guessed. It does
not modify media files and writes no manifest rows.

Run as a dedicated unprivileged service user in production. See
[operations](docs/operations.md) for the full key reference, a minimal example,
and the systemd and container deployment shapes.

## How idempotency works

Three independent mechanisms, so losing one does not cause a second lossy
generation:

1. The re-encode threshold (86) sits above the fixed output quality (85), so
   ordinary lossy output is not a re-encode candidate. `target` mode settles at
   or below 85 for most files, but a file that needs a higher quality to hold
   the floor can stay above it. For those files the COM marker and the manifest
   are the guarantee, not the threshold.
2. Every JPEG the tool writes carries a COM marker
   `mediaopt:<version>:<qNN|ll>`. The scanner checks for it before deciding, so
   a restored or moved file is recognised as already processed.
3. The manifest stores the fingerprint of the file after the swap. The next run
   compares that fingerprint and skips the file without reading it. Rejected
   results are recorded too, so a file with no achievable gain is not encoded
   again every night.

The 220-file pilot confirmed this: 165 changed, 55 rejected, 19.15 MiB freed,
then the second and third runs changed 0 files.

## Safety guarantees

- **Atomic replace.** The output is written to a temporary file in the same
  directory, fsynced, given the original mode, owner, group, mtime and atime,
  renamed over the original, and the directory is fsynced. A reader sees the old
  file or the new file, never a partial one.
- **Verification gate.** The output must decode through `vipsheader` at the
  expected dimensions before it can replace anything. In `target` mode the
  measured SSIMULACRA2 score must also meet the floor.
- **Gain gate.** A lossy result is only kept when it is at least
  `jpeg.min_gain_percent` (default 5%) smaller; a lossless result only has to be
  smaller by `jpeg.min_gain_percent_lossless` (default 0.5%). Anything else is
  rejected and the original stays.
- **Bounded runs.** Per-run wall clock, replaced bytes and changed files are
  capped; a root is skipped when free space is below the reserve; a lock stops a
  second instance; the process runs at low priority (nice 19 where permitted)
  and, on Linux, idle I/O.
- **No surprises.** The format is sniffed from magic bytes and no file is ever
  renamed. 14.3% of files named `.png` in one production tree were actually
  JPEG, which is why the extension is only used as a cheap first filter.

`mediaopt verify` re-checks a random sample of changed files against the
manifest. See [rollback](docs/rollback.md) for the restore procedure.

## Why not X

- **picopt** is lossless-only. It cannot recover the 71% that a quality-targeted
  re-encode frees on q96-100 sources, or the 15.5-17.5% on q88-90 sources.
- **jpeg-archive / SmallFry** is unmaintained and its metric is broken on recent
  builds. Its SSIM target has almost no usable resolution: SSIM 0.9999
  corresponds to SSIMULACRA2 74 and SSIM 0.995 to SSIMULACRA2 43.
- **squoosh-cli** is dead. It is not maintained and does not fit an unattended
  nightly job.

## Documentation

- [Status](docs/STATUS.md): where the rollout stands right now — what works, the
  decisions taken, what is open, and the next step.
- [Architecture](docs/architecture.md): modules, data flow, manifest schema,
  recovery, threading and budgets.
- [Operations](docs/operations.md): configuration, deployment, scheduling,
  logging, commands, monitoring and runbooks.
- [Formats](docs/formats.md): quality, SSIMULACRA2, subsampling, metadata, and
  why each format is handled the way it is.
- [Measurements](docs/measurements.md): method, tables and reproduction.
- [Migration](docs/migration.md): replacing a cron-driven optimiser.
- [Rollback](docs/rollback.md): detection, stop, restore and manifest repair.

Repository: <https://github.com/HansVanEijsden/media-optimizer>.
