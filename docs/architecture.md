# Architecture

`mediaopt` is a single binary with no daemon and no network code. This document
describes how a run moves from a directory walk to a manifest row, and which
invariants each module is responsible for.

## Design constraints

- **Idempotent.** A second run over an unchanged tree must change nothing. The
  re-encode threshold, the COM marker and the manifest all exist to enforce that.
- **Safe on a live tree.** Files are replaced atomically, ownership and
  timestamps are preserved, and no file is renamed or converted.
- **Bounded.** Every run has a wall-clock, file and byte budget, a free-space
  check per root, and a single-instance lock.
- **Recoverable.** State lives in SQLite plus an exportable JSON snapshot, and
  every JPEG the tool writes can identify itself without the manifest.

## Module map

| Module | Responsibility |
| --- | --- |
| `config.rs` | TOML schema, defaults and validation; one file per host |
| `probe.rs` | Magic-byte sniffing and header-only probe; reads at most 192 KiB per file |
| `jpeg.rs` | JPEG marker parser, quality estimate, COM marker insert and strip |
| `png.rs` | PNG and WebP header parsers |
| `decide.rs` | Pure decision function: one action and an optional reason per file |
| `engine.rs` | Encoder and measurement subprocesses, header reads, decode verification |
| `run.rs` | Walk, fast path, per-file pipeline, budgets, run bookkeeping |
| `replace.rs` | Temporary paths, metadata, atomic rename, SHA-256, stale temp cleanup |
| `manifest.rs` | SQLite schema, queries, export and import, prune and vacuum |
| `limits.rs` | Run lock, low priority, free space, budget accounting, byte formatting |
| `sched.rs` | Daily schedule parser, catch-up, signal handling |
| `report.rs` | Human summary, one-line summary, per-run JSON, report pruning |
| `main.rs` | clap command line and command wiring |

## Data flow

```text
scan -> probe -> decide -> encode -> verify -> replace -> record
```

1. **Scan.** `run` acquires the lock, applies low priority, opens the manifest
   and inserts a `runs` row. For each enabled root it checks the directory and
   the free-space reserve, removes stale temporary files it finds in the root
   directory, then walks with `WalkDir` without following symlinks. `.git`,
   `.svn`, `node_modules`, `.cache`, `.trash`, `@eaDir` and `#recycle` are
   pruned. Extensions `jpg`, `jpeg`, `jpe`, `jfif`, `png`, `webp` and `gif` pass
   the first filter; configured `exclude` substrings are matched against the
   full path.
2. **Fast path.** For each regular file the walker takes a fingerprint of
   `(size, mtime_ns, inode)` and compares it with the manifest. If the
   fingerprint matches and the recorded outcome is not `pending`, the file is
   skipped without being opened. This is where idempotency pays for itself.
3. **Probe.** The file's head is read and the format is taken from the magic
   bytes. JPEG, PNG and WebP headers are parsed without decoding pixels. For a
   JPEG, the parser also reports the stored quality estimate, its confidence,
   subsampling, component count, metadata segments and any COM markers.
4. **Decide.** `decide::decide` is a pure function of the probe, the file size,
   the file age, the global config and the root policy. It returns exactly one
   action and, for a skip, one machine-readable reason. No I/O happens here.
5. **Encode.** Output always goes to a temporary file in the same directory:
   `.<name>.mediaopt.<pid>.<counter>.tmp`. This keeps the final rename inside one
   filesystem. The action selects the encoder:
   - `jpeg_reencode`: `vips jpegsave`, or `vips resize` when a downscale is
     needed. In `target` mode a quality ladder re-encodes until the floor is
     missed and keeps the lowest quality that met it.
   - `jpeg_lossless`: `jpegtran -copy all -optimize -progressive`.
   - `png_quant`: `pngquant`, then `oxipng` on the palette output. If pngquant
     declines the quality floor, the lossless path is used instead.
   - `png_lossless`: `oxipng` in process.
6. **Verify.** The temporary file must decode through `vipsheader` at the
   expected dimensions, allowing one pixel of rounding from the scaler. Only
   then is the COM marker inserted (JPEG only), because the marker changes the
   byte size that the gain gate sees. The gain gate and, for lossy results, the
   SSIMULACRA2 floor are checked after the marker.
7. **Replace.** Mode, owner, group, mtime and atime are applied to the temporary
   file, it is renamed over the original, and the parent directory is fsynced.
8. **Record.** The manifest row is written with the post-swap fingerprint, the
   SHA-256 of the replaced file, the action, the settings, the measured score and
   the outcome.

A dry run (`plan` or `run --dry-run`) executes the same pipeline, including the
real encoder, and removes the temporary file after the gates. It writes no media
files and no manifest rows; the per-root projection comes from in-memory
counters.

## Manifest schema

`SCHEMA_VERSION` is 2. The database is SQLite in WAL mode with a 30-second busy
timeout and foreign keys on. A `flock` on `<db>.lock` prevents overlapping runs
in the first place.

### `files`

One row per absolute path that has ever been considered.

| Column | Meaning |
| --- | --- |
| `path` | absolute path, primary key |
| `root` | configured root the path belongs to |
| `size_before`, `mtime_ns`, `inode` | identity of the input at the time of the decision, used for reporting |
| `fp_size`, `fp_mtime_ns`, `fp_inode` | fingerprint of the file after the tool finished with it, used by the fast path |
| `size_after` | size of the replacement, null when nothing was replaced |
| `sha_before`, `sha_after` | SHA-256 content identity, see below |
| `content_format`, `width`, `height` | probe result |
| `quality`, `quality_confident` | stored-quality estimate and whether its table matched IJG closely |
| `action`, `reason` | machine-readable decision, for example `jpeg_reencode` or `quality_already_lean` |
| `ssimu2` | measured score of the kept result, when measured |
| `encoder`, `encoder_version`, `settings` | what wrote the file and with which options |
| `outcome` | `pending`, `changed`, `rejected`, `skipped` or `error` |
| `detail` | human-readable explanation, including the before and after size |
| `run_id` | the run that last wrote the row |
| `ts` | when the row was last written |

Indexes exist on `sha_before`, `sha_after`, `outcome` and `root`.

### `runs` and `meta`

`runs` records one row per invocation: id, start and finish timestamps, host,
tool version, mode (`apply`, `dry-run`, `scheduled`, `test`), counters and
byte totals. `meta` is a key-value table holding `schema_version` and
`last_host`.

### Why `fp_*` columns

After a replacement the file is a different inode with a different size. The
`size_before`, `mtime_ns` and `inode` columns deliberately keep the identity of
the input so reports can say what was replaced. The fast path, however, has to
compare the file as it is now. Without `fp_*`, every file the tool had changed
would fail its own fingerprint check on the next run, be re-probed, and be
reconsidered every night. `fp_*` is copied from the post-swap `stat`, so the
next run recognises its own work. For skipped and rejected rows, `fp_*` simply
equals the input values because the file did not change.

### Content identity

In 0.1.0 a changed row stores the SHA-256 of the file that replaced the
original in both `sha_before` and `sha_after`, because the original bytes no
longer exist when the row is written. `find_by_hash` accepts either column, so
content identity survives a restore even when paths and inodes do not. The scan
path in 0.1.0 does not call it; it relies on the manifest path row and the COM
marker. The hash pair is part of the export snapshot and of the recovery design.

## Recovery story

A full restore onto new hardware changes every inode and mtime, and may change
the mount path. The tool has five independent layers:

| Mechanism | Survives a restore | Used automatically in 0.1.0 |
| --- | --- | --- |
| manifest path row | no, when the path changes | yes, when the path is unchanged |
| `fp_*` fingerprint | no, inode and mtime change | yes |
| SHA-256 content hash | yes | only through `find_by_hash`, not called by the scan |
| the `mediaopt:` COM marker (with `q<NN>` or `ll`) | yes | yes, before deciding |
| gzipped manifest snapshot from `export` | yes, if stored off the machine | only through a manual `import` |

After a restore, a JPEG that the tool wrote is recognised by its COM marker and
recorded as `already_processed` without being encoded again. PNG files carry no
marker, so their idempotency depends on the manifest or the snapshot. If the
manifest is lost, restore the latest `export` snapshot with `mediaopt import`;
existing rows win, so importing into a live manifest is safe. See
[rollback](rollback.md).

## Threading, budgets and priority

- `limits.jobs` defaults to the smaller of the available parallelism and 2. It
  sizes a global Rayon pool; `WalkDir::par_bridge` feeds files into it.
- The manifest connection and the budget counters sit behind mutexes. File
  encoding is a subprocess, so worker threads mostly wait on processes rather
  than burn CPU.
- Before probing a file the worker checks the budget: wall clock, files changed,
  bytes replaced. The byte budget counts the input size of each replaced file,
  not the bytes written, so it bounds how much of the tree can change in one run.
- When a limit is hit, a stop flag is set. The file in flight finishes and is
  recorded, and the rest of the tree is left for the next run. The run row gets a
  note such as `stopped early: max_files_per_run`.
- Low priority is applied to the process itself, so every encoder subprocess
  inherits it: nice 19 where permitted, and idle I/O class on Linux.
- A root whose filesystem has less free space than `limits.free_space_reserve`
  is skipped with a warning.

## Why subprocess engines

The tool has to run unchanged on an Apple Silicon laptop and inside a Debian
container. Linking libvips built against mozjpeg would make the binary depend on
one specific shared-library build. Invoking the CLI instead makes the encoder a
deployment choice. The measured cost is throughput: 3.54 images/s through the
vips CLI against 4.14 in process on a 2048x1366 photo. That is a fair price for
portability and for the fact that a libvips-mozjpeg package, not a Rust FFI
dependency, is what has to be shipped.

Tool discovery is ordered: explicit path in `[engine]`, then the matching
environment variable (`VIPS_BIN`, `JPEGTRAN_BIN`, `PNGQUANT_BIN`,
`SSIMULACRA2_BIN`), then `PATH`. A binary that exists but cannot run is treated
as missing, with a warning, so a broken extraction fails loudly instead of
encoding garbage. `vips` and `jpegtran` are mandatory; the others are optional
with the consequences described in [operations](operations.md).

## Known gaps in 0.1.0

- `webp.lossless_only` leads to a `webp_lossless` decision, but the encoder
  returns "not implemented in v1" and the file is recorded as an error.
- `limits.oldest_first`, `png.dither`, the per-root byte and file budgets,
  `state.log_file` and the per-root `keep` list are parsed but not applied.
- An encode error records a row whose fingerprint still matches the unchanged
  file. Because the fast path skips every non-pending row with a matching
  fingerprint, that file is only retried after it changes. There is no CLI
  command in 0.1.0 to clear a single row.
- Metadata and timestamp handling uses Unix APIs; the tool targets Unix hosts.
