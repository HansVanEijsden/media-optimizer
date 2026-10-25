# Changelog

All notable changes to this project are documented in this file. The format is
based on Keep a Changelog, and the project follows semantic versioning.

## [0.1.0] - unreleased

The first release. `mediaopt` is a single binary that optimises large, live
image trees in place, with a manifest that makes repeated runs a no-op.

### Added

- TOML configuration with global `[jpeg]`, `[png]`, `[webp]`, `[limits]`,
  `[state]` and `[engine]` sections and a list of `[[roots]]`, each with its own
  policy, owner, exclusions and optional budgets. Unknown keys are rejected.
- Header-only probing of JPEG, PNG and WebP. The format is taken from magic
  bytes; files are never renamed and extensions are only a first filter.
- Per-file decisions with a stable machine-readable reason. Rules cover stored
  quality, dimension limit, CMYK, PNG alpha, palette and ICC, lossy WebP and
  unhandled formats.
- JPEG re-encoding through libvips with the density-critical saver options:
  optimal coding, progressive, trellis quantisation, deringing, optimised scans
  and quantisation table 3.
- `target` quality mode: a per-file search for the lowest quality that still
  meets an SSIMULACRA2 floor, with a +3 threshold margin when the stored-quality
  estimate is not confident.
- `fixed` quality mode for predictable output.
- Lossless JPEG transcoding with `jpegtran -copy all -optimize -progressive`
  and a 0.5% gain gate.
- Downscaling to `jpeg.max_dimension` with a configurable libvips kernel,
  defaulting to Catmull-Rom (`cubic`).
- Chroma subsampling policy `auto`, `on` or `off`; 4:4:4 sources stay 4:4:4.
- Palette PNG quantisation with `pngquant` and a quality floor, followed by
  `oxipng` on the palette output. Transparent, palette and ICC-bearing PNGs go
  to the lossless path instead.
- Lossless PNG recompression with `oxipng` in process, stripping nothing.
- Atomic replacement: temporary file in the same directory, fsync, preserved
  owner, group, mode, mtime and atime, rename, directory fsync.
- Verification gate: every output must decode at the expected dimensions, and
  every scored lossy result must meet the SSIMULACRA2 floor, before it replaces
  anything.
- SQLite manifest with fingerprints, content hashes, probe results, decisions,
  measured scores, encoder versions and outcomes, plus a `runs` table.
- Idempotency: manifest fast path, `mediaopt:<version>:<qNN|ll>` COM marker on
  every JPEG the tool writes, and a re-encode threshold above the fixed output
  quality.
- Per-run budgets for wall clock, replaced bytes and changed files, a free-space
  reserve per root, a single-instance lock, low priority (nice 19 where
  permitted) and idle I/O on Linux.
- Daily scheduler with catch-up after a missed night, built-in SIGTERM and
  SIGINT handling and `MEDIAOPT_SCHEDULE` configuration.
- Commands `run`, `plan`, `report`, `tools`, `verify`, `export`, `import`,
  `compact` and `schedule`.
- Per-run JSON reports, a one-line summary for log scraping, and report
  retention through `state.report_days`.
- Manifest export to gzipped JSON and additive import from a snapshot.

### Known limitations

- Lossless WebP re-packing is recognised but not implemented; such files are
  recorded as errors.
- `limits.oldest_first`, `png.dither`, `state.log_file`, the per-root budgets
  and the per-root `keep` list are accepted by the config but not applied.
- GIF, AVIF, HEIC, TIFF, BMP, JPEG XL and CMYK JPEGs are out of scope.
- The tool targets Unix hosts.
