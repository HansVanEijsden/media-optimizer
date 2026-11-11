# Status

_Snapshot taken 2026-11-11. This page is the operational state: what works, what
was decided, what is open, and what to do next. The design lives in
[architecture.md](architecture.md), the format details in
[formats.md](formats.md), the measurements behind the defaults in
[measurements.md](measurements.md) and the routine procedures in
[operations.md](operations.md)._

## Where it runs

Four hosts, all on `ghcr.io/hansvaneijsden/media-optimizer:sha-1601735` (built
2026-11-11, 86 MB, digest `sha256:73c864…`; amd64-only for now — the emulated
arm64 build stalled once and is retried separately), each with its own TOML file
and its own nightly slot. The image pulls anonymously from GHCR; the binary
inside all four containers is byte-identical. The legacy cron optimiser is gone
from every host: no `/root/tools`, no `/root/optimize_images.sh`, no disabled
crontab lines and no local build directories. The container is the only
optimiser left. Host names, client names and paths are deliberately absent from
this repository; the labels below are the ones used everywhere else in the docs.

| label | what it is | roots | nightly slot | burn-down |
|---|---|---|---|---|
| web-a | large static image tree (news site) plus two WordPress upload trees | 4 | 03:30 | yes |
| web-b | shared host, 26 WordPress uploads plus one photo tree | 27 | 04:15 | yes |
| web-c | VPS with a static image tree | 1 | 03:15 | no |
| web-d | one small WordPress install | 1 | 04:45 | yes |

One of web-a's four roots is an empty placeholder; it walks nothing and costs
nothing. The backlog is gone on all four hosts: the nightly runs now change only
what new uploads add.

## What works

- **The pipeline.** `scan → probe → decide → encode → verify → replace → record`,
  as described in [architecture.md](architecture.md). Every replacement is
  atomic, keeps ownership, mode and mtime, and is recorded in a SQLite manifest
  plus a COM marker inside the JPEG.
- **Idempotency.** A second run over an optimised tree changes nothing; a run
  over a partly done tree resumes per file, not per directory, because the
  manifest is the cursor. A crash or a `docker stop` costs at most the file that
  was in flight.
- **Quality model.** JPEG in `target` mode: the stored quality is re-encoded to
  the lowest quality that still meets the SSIMULACRA2 floor (75), with a
  three-point margin when the source quality has to be estimated from the
  quantisation tables. Images above `max_dimension` (2048) are downscaled with
  the `cubic` kernel. When a result gains less than the margin, the original is
  kept and that decision is remembered.
- **Formats.** JPEG re-encode, JPEG lossless (`jpegtran -optimize -progressive
  -copy all`), JPEG downscale, PNG palette quantisation when safe, PNG lossless
  via `oxipng`, WebP lossless only. The format is sniffed from magic bytes: on a
  real tree 14% of files named `.png` are JPEGs.
- **Resource discipline.** `nice 19`, idle I/O class, two jobs on a three-core
  host, a single-instance lock, a free-space reserve, per-run byte/file budgets
  and one wall-clock deadline for the whole run — a root may tighten the byte and
  file allowance, never the clock — and a nightly slot with a 90-minute catch-up
  window. A run honours SIGTERM, so `docker stop` takes seconds instead of the
  full grace period.
- **Remote mode.** `scan` on the server, `encode-batch` on a fast machine,
  `apply-staged` back on the server, which re-verifies every result before it
  installs it. Used to clear a 581,000-file backlog from a laptop at roughly ten
  times the speed of the server itself; the script is
  `scripts/mediaopt-remote.sh`.
- **Delivery.** Multi-stage Dockerfile, non-root, read-only root filesystem,
  all capabilities dropped, per-host compose file. CI runs `fmt`, `clippy` and
  the tests (74 unit, 11 integration); the release workflow builds only when
  code, the Dockerfile or the Cargo manifests change, so a documentation commit
  never produces an image. It builds amd64 and emulated arm64; a manual
  `platforms` input publishes amd64-only when the arm64 build stalls, which it
  did once on 2026-11-11 (45 minutes in the rust stage, no progress).

## Results so far

These are the manifest totals per host, the only number that matters: bytes on
disk before versus after, for files the tool actually replaced.

| label | files changed | space freed | share of those bytes | state |
|---|---:|---:|---:|---|
| web-a | 485,266 | **41.5 GiB** | 33% | settled, nightly re-check of new uploads |
| web-c | 414,782 | **16.9 GiB** | 25% | settled, nightly re-check |
| web-b | 94,655 | **1.7 GiB** | 14% | settled, nightly re-check |
| web-d | 2,260 | **27.6 MiB** | 18% | settled, nightly re-check |
| **total** | **996,963** | **~60 GiB** | | |

For comparison, the nightly image backup of these hosts is the rollback path and
runs before the optimiser, so the freed space lands in the following night's
snapshot.

Two other things worth recording:

- The tool's own footprint is known: the manifest is roughly 500 bytes per row
  two weeks in — 349 MB for 709,000 rows on web-a, 264 MB and 87 MB on web-c and
  web-b, 1.4 MB on web-d — the image is 86 MB, and no temporary files survive a
  run. That last property was measured, not assumed: the reject path removes its
  own output, a run sweeps anything older than thirty minutes, and an integration
  test holds it.
- Every host has **zero** or near-zero files in the `error` state. The handful
  that remain are truncated uploads (`Premature end of JPEG file`, PNG files that
  are an exact multiple of 4096 bytes), which the tool correctly refuses to
  touch and retries once a night at negligible cost.

## Decisions

1. **One Rust binary, no runtime dependencies beyond the engines.** libvips,
  mozjpeg's `jpegtran`, `pngquant` and `ssimulacra2`; everything else is in the
  binary. Licence MIT OR Apache-2.0.
2. **Never rename, never convert, never strip.** The file stays where it is, in
  the format it already had, with ICC and EXIF kept. This is what makes the tool
  safe on a live site: no URL changes, no broken references.
3. **SSIMULACRA2 is the quality target**, not PSNR or a fixed quality number.
  Floor 75 at the soft end, per-file search in `target` mode. The three-point
  margin exists because the source quality can only be estimated from the
  quantisation tables, and guessing optimistic would degrade the file.
4. **2048 px longest side**, `cubic` (Catmull-Rom) for the downscale: 4% smaller
  than lanczos3 at the same metric score.
5. **mozjpeg quality via libvips**, which must be built against mozjpeg. With
  `quant-table=3` libvips matches `cjpeg` within 0.1%; the default table is
  2.9-5.2% fatter.
6. **PNG: palette only when it is safe.** Opaque, no ICC profile, and the
  quantised result has to clear a quality floor of 80. Everything else gets a
  lossless `oxipng` pass. Palette quantisation measured 53-68% on suitable files.
7. **WebP lossless only.** Lossy WebP is skipped rather than silently accepted.
8. **No JPEG XL in version 1.** The serving stack already produces JXL per
  request through imgproxy, so an on-disk JXL copy buys nothing today; the
  archive pilot is deferred until the current rollout is finished.
9. **Idempotency by record, not by marker.** A SQLite manifest holds the
  decision per file (fingerprint, outcome, configuration digest), and the COM
  marker is only a secondary check. The legacy marker of the old cron job is
  never trusted: 99.4% of one tree carried it while 20% of the files were still
  stored at quality 95-100.
10. **Rollback is the existing nightly VM image backup.** No archive feature in
    the tool, no second copy of the tree, no undo log. This is why the optimiser
    must run after the backup.
11. **Remote mode for large backlogs**, with the server still verifying and
    recording everything. The laptop never touches the manifest directly.
12. **Two of three cores, `nice 19`, idle I/O.** Interactive traffic always wins,
    and that has held during the whole catch-up.
13. **`min_file_size` 5120 by default**, 32768 on the host where thumbnail trees
    dominate; small thumbnails cannot gain 5% and only cost time.
14. **Everything may be documented publicly, except hostnames, client names,
    personal data and secrets.** The name list lives outside the repository and
    `scripts/check-no-secrets.sh` reads it from there; the checker itself is
    generic and contains no names.
15. **The site's own generated sizes stay in scope.** web-a's `static/img` holds
    not only the yearly originals but the site's derived variants (`normal-b`,
    `normal-b@2x`, `square128`, `square280`, `fb`, `thumb` and their `-nc`
    siblings): 291,800 files, 26 GB. Measured 2026-11-11 they are 61% of that
    tree's manifest rows and 17% of its savings (6.2 GiB), against 33,700 rows
    that settle as rejected. The site still adds ~250 uploads a month, ~1,500 new
    variants, worth roughly 0.5 GB a year. Excluding them would save one or two
    minutes of the nightly scan and forfeit that, so they stay.
16. **A broken image is reported once, then settled.** A file whose bytes cannot
    be decoded is recognised from the loader's error message and recorded as a
    settled skip with reason `unreadable`; it is retried only when its
    fingerprint changes, so a repaired upload is picked up on the next run.
    Lossless WebP, whose re-pack is not implemented in 0.1.0, is settled the same
    way with reason `webp_lossless_unimplemented` instead of failing every night.
    Before this, web-a carried a permanent `errors=` of 18: two truncated JPEGs
    and sixteen lossless WebP files.

## Open items

1. **Files with names that are not valid UTF-8 are skipped in remote mode.** The
   transport is byte-safe, but APFS cannot store such a name, so the client skips
   them and the server's own run handles them. One or two per tree. Documented in
   `operations.md`.
2. **The manifest keeps growing.** Roughly 500 bytes per row: 349 MB for 709,000
   rows on web-a, 264 MB on web-c. Most of that is the fast-path cache for files
   that are settled and will never change again, so pruning saves time at the
   cost of a re-probe later. There is no last-seen column — the fast path
   deliberately does not write, so `ts` is the decision time, not the last
   sighting — and rows of files that were deleted cannot be told apart from
   healthy settled rows. Worth a design decision before the largest host passes a
   gigabyte.
3. **JPEG XL archive pilot** (phase 7) and a **deploy webhook for the site
   generator** (phase 8) are designed but not started.

## Next step

1. Item 2: design the manifest pruning policy — with the measured growth rate
   from the snapshot above, and the caveat that a pruned row costs a re-probe.
2. Item 3: the JPEG XL pilot and the deploy webhook stay deferred.

## How to check the current state

```console
# one-line summary per host, straight from the manifest
mediaopt report

# what the last runs did
docker logs mediaopt | grep -E '^mediaopt (run|dry-run)'

# any run still in progress, and where it is
docker logs mediaopt | grep 'progress:' | tail -1

# confirm the tool left no temporary files behind
find /path/to/root -name '*.mediaopt.*.tmp' -type f -printf '%TY-%Tm-%Td %TH:%TM  %p\n'
```

A temporary file that is seconds old belongs to a running encode. A temporary
file from yesterday is a bug: the reject path removes its own output, and a run
also sweeps anything older than thirty minutes at each root.
