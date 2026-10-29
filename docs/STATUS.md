# Status

_Snapshot taken 2026-10-29. This page is the operational state: what works, what
was decided, what is open, and what to do next. The design lives in
[architecture.md](architecture.md), the format details in
[formats.md](formats.md), the measurements behind the defaults in
[measurements.md](measurements.md) and the routine procedures in
[operations.md](operations.md)._

## Where it runs

Four hosts, all on the same 0.1.0 image (built 2026-10-27, 86 MB), each with its
own TOML file and its own nightly slot. That image predates the budget fix of
2026-10-29, so the per-run semantics described below are `main`, not yet the
running containers. Host names, client names and paths are deliberately absent
from this repository; the labels below are the ones used everywhere else in the
docs.

| label | what it is | roots | nightly slot | burn-down |
|---|---|---|---|---|
| web-a | one large static image tree (news site) | 5 | 03:30 | yes, catch-up in progress |
| web-b | shared host, 25+ WordPress installs | 25 | 04:15 | yes |
| web-c | VPS with a static image tree | 1 | 03:15 | no |
| web-d | one small WordPress install | 1 | 04:45 | yes |

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
  the tests (71 unit, 9 integration); the release workflow builds only when
  code, the Dockerfile or the Cargo manifests change, so a documentation commit
  never produces an image.

## Results so far

These are the manifest totals per host, the only number that matters: bytes on
disk before versus after, for files the tool actually replaced.

| label | files changed | space freed | share of those bytes | state |
|---|---:|---:|---:|---|
| web-a | 468,076 | **39.8 GiB** | 33% | still working through the backlog |
| web-c | 409,978 | **16.4 GiB** | 25% | settled, nightly re-check |
| web-b | 94,478 | **1.7 GiB** | 14% | settled, nightly re-check |
| web-d | 2,260 | **27.6 MiB** | 18% | settled, nightly re-check |
| **total** | **974,792** | **~58 GiB** | | |

For comparison, the nightly image backup of these hosts is the rollback path and
runs before the optimiser, so the freed space lands in the following night's
snapshot.

Two other things worth recording:

- The tool's own footprint is small and known: the manifest is roughly 350 bytes
  per row (18-63 MB on these hosts), the image is 86 MB, and no temporary files
  survive a run. That last property was measured, not assumed: the reject path
  removes its own output, a run sweeps anything older than thirty minutes, and an
  integration test holds it.
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

## Open items

1. **Legacy tooling is still installed on three hosts**: `/root/tools` and
   `/root/optimize_images.sh`. The crontab entry is commented out everywhere with
   a `# DISABLED by mediaopt` note, the old containers are gone and the old image
   is removed. Decision needed: delete the remaining files or keep them for
   reference.
2. **Files with names that are not valid UTF-8 are skipped in remote mode.** The
   transport is byte-safe, but APFS cannot store such a name, so the client skips
   them and the server's own run handles them. One or two per tree. Documented in
   `operations.md`.
3. **Undecodable files are retried every night.** Cheap (a second per file), but
   a permanently broken upload will warn forever. Marking a decode failure as
   settled, the way a rejection is settled, would keep the nightly report clean.
   Documented in `operations.md` as intended behaviour for now.
4. **The manifest grows without bound in principle.** Roughly 350 bytes per row,
   so a million-file host is a 350 MB database. Fine today; worth a pruning
   policy (drop rows for files that have not been seen for a year) before it
   reaches a million.
5. **Thumbnail trees on web-a are still processed** (`min_file_size` 5120 there).
   51,000 rejections on those trees so far. Deliberately kept: the owner wanted
   to see whether anything can be won there before excluding them.
6. **JPEG XL archive pilot** (phase 7) and a **deploy webhook for the site
   generator** (phase 8) are designed but not started.

## Next step

1. Rebuild the image and roll it out: all four hosts still run the 2026-10-27
   build, so the per-run budget semantics only become real once they do.
2. Decide on item 1 (legacy files) and item 5 (thumbnail trees); both are
   housekeeping rather than engineering.
3. Then item 3: settle a file that fails to decode, so the nightly `errors=` line
   only shows new problems.
4. Before any host approaches a million files, add the manifest pruning policy of
   item 4. The JPEG XL pilot and the deploy webhook (item 6) stay deferred until
   the web-a backlog is finished.

## How to check the current state

```console
# one-line summary per host, straight from the manifest
mediaopt report --totals

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
