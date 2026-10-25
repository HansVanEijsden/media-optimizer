# Operations

This document covers configuration, deployment, scheduling, logging, commands, monitoring and
the three runbooks an operator is most likely to need.

## Configuration
The config is one TOML file per host. It is read from `--config`, then `$MEDIAOPT_CONFIG`, then
the first existing of `/etc/mediaopt/config.toml`, `mediaopt.toml` and `config.toml`. Unknown
keys are rejected, so a typo is an error rather than a silently ignored setting. Every root path
must be absolute. The defaults below apply when a key is omitted.

### `[jpeg]`

| Key | Default | Meaning |
| --- | --- | --- |
| `quality_mode` | `"target"` | `target` searches the lowest quality that meets `ssimu2_floor`; `fixed` encodes once at `fixed_quality` |
| `ssimu2_floor` | `75.0` | SSIMULACRA2 floor for `target`, validated 60 to 100 |
| `fixed_quality` | `85` | quality in `fixed` mode, and the starting point of the search |
| `reencode_min_quality` | `86` | stored quality at or above this triggers a lossy re-encode; 3 is added when the estimate is not confident |
| `max_dimension` | `2048` | longest side after downscaling; 0 disables resizing |
| `resize_kernel` | `"cubic"` | libvips kernel name; `cubic` is Catmull-Rom |
| `min_gain_percent` | `5.0` | a lossy result must be at least this much smaller to be kept |
| `lossless_pass` | `true` | run `jpegtran` on JPEGs that are not re-encoded |
| `min_gain_percent_lossless` | `0.5` | a lossless result must be at least this much smaller |
| `subsample` | `"auto"` | `auto` keeps 4:4:4 sources at 4:4:4, else 4:2:0; `on` forces 4:2:0; `off` forces 4:4:4 |
| `keep` | `["icc", "exif"]` | metadata passed to libvips as `keep`; an empty list strips metadata on re-encode |

### `[png]`, `[webp]`

| Key | Default | Meaning |
| --- | --- | --- |
| `png.mode` | `"quant_if_safe"` | `skip`, `lossless` or `quant_if_safe` |
| `png.quality_floor` | `80` | pngquant floor; below it the palette result is discarded and the lossless path is used |
| `png.min_gain_percent` | `5.0` | palette result must be at least this much smaller |
| `png.lossless_effort` | `2` | oxipng preset, 0 to 6 |
| `webp.lossless_only` | `true` | lossless WebP is considered, but the action is not implemented in 0.1.0 and is recorded as an error |

### `[limits]`, `[state]`, `[engine]`

| Key | Default | Meaning |
| --- | --- | --- |
| `limits.jobs` | min(parallelism, 2) | worker threads and Rayon pool size |
| `limits.max_runtime` | `"5h"` | wall clock; the file in flight finishes, then the run stops |
| `limits.min_file_age` | `"30m"` | newer files are skipped and not recorded |
| `limits.free_space_reserve` | `"2GiB"` | a root is skipped when the filesystem has less free space |
| `limits.min_file_size` | `5120` | smaller files are never considered |
| `limits.max_bytes_per_run` | `0` | input bytes replaced per run; 0 means unlimited |
| `limits.max_files_per_run` | `0` | encode attempts per run; 0 means unlimited |
| `state.db` | `/var/lib/mediaopt/state.db` | SQLite manifest, never inside a media tree |
| `state.report_dir` | `/var/lib/mediaopt/reports` | one JSON file per run |
| `state.log_file` | unset | write a daily-rotated log file; `--log-file` overrides it |
| `state.report_days` | `90` | reports older than this are pruned after a run; 0 disables pruning |
| `engine.vips` | `$VIPS_BIN`, then `PATH` | libvips CLI, required |
| `engine.jpegtran` | `$JPEGTRAN_BIN`, then `PATH` | mozjpeg `jpegtran`, required |
| `engine.pngquant` | `$PNGQUANT_BIN`, then `PATH` | needed for the palette PNG path |
| `engine.ssimulacra2` | `$SSIMULACRA2_BIN`, then `PATH` | without it, `target` falls back to one encode at `fixed_quality` |

### `[[roots]]`

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | required | absolute directory |
| `policy` | `"optimise"` | `optimise` or `lossless_only` |
| `enabled` | `false` | a root must be enabled explicitly |
| `owner` | unset | `user:group` for every replaced file; unset preserves the original owner |
| `keep` | `["icc", "exif"]` | accepted on `lossless_only` roots but not applied in 0.1.0 |
| `managed_by_remote` | `false` | skipped by this process; `mediaopt tools` lists it |
| `max_bytes_per_run`, `max_files_per_run` | unset | accepted but not enforced in 0.1.0; only global limits apply |
| `exclude` | `[]` | substrings matched against the full path |

### Minimal example

```toml
[state]
db = "/var/lib/mediaopt/state.db"
report_dir = "/var/lib/mediaopt/reports"

[[roots]]
path = "/home/example/www/static/img"
owner = "example:example"
enabled = true
```

All other keys keep their defaults. Engine paths can be omitted when the binaries are on `PATH`.

## Deployment

Run `mediaopt schedule` under systemd, or as the command of a container. It is one long-lived
process with an internal daily timer.

### systemd

```ini
[Unit]
Description=mediaopt image optimiser
After=local-fs.target

[Service]
Type=simple
User=mediaopt
Group=mediaopt
ExecStart=/usr/local/bin/mediaopt --config /etc/mediaopt/config.toml schedule
Environment=MEDIAOPT_SCHEDULE=04:15
Restart=on-failure
RestartSec=5m
NoNewPrivileges=true
ProtectSystem=strict
PrivateTmp=true
ReadWritePaths=/var/lib/mediaopt /mnt/media
TimeoutStopSec=6h
```

Add every configured root to `ReadWritePaths`, including roots under `/home`, and run the service
as a dedicated account that can write the media roots and the state directory. `TimeoutStopSec`
must exceed `limits.max_runtime`, because a stop request does not abort the run in flight.

### Container

```console
docker run -d --name mediaopt \
  --restart unless-stopped \
  -e MEDIAOPT_SCHEDULE=04:15 \
  -e TZ=UTC \
  -v /etc/mediaopt:/etc/mediaopt:ro \
  -v /mnt/media:/media \
  -v /srv/mediaopt:/var/lib/mediaopt \
  ghcr.io/hansvaneijsden/media-optimizer:0.1.0 \
  schedule
```

Keep the state volume persistent. The image entrypoint is the `mediaopt` binary, so the last
argument is the subcommand. `TZ` sets the clock for the schedule and for report timestamps. Roots
marked `managed_by_remote` are skipped by this process.

## Scheduling and catch-up

`schedule` reads `MEDIAOPT_SCHEDULE` (default `04:15`). It accepts `HH:MM` or a daily five-field
form `M H * * *`; ranges, steps and weekday fields are rejected.

At startup the scheduler looks at the last finished run. If that run is more than 90 minutes past
the expected next occurrence, it runs immediately instead of waiting for the next slot, so a host
that was down at the scheduled minute does not lose a night. If no run has ever finished, the
first run starts immediately. After that it sleeps until the next daily slot.

The sleep loop wakes every 30 seconds to check for shutdown. `SIGTERM` and `SIGINT` set a flag. A
run in flight is not interrupted mid-run: it finishes and records itself, then the scheduler
exits. A stop during a long run therefore waits, or the process is killed. Each file is replaced
atomically, so a kill never leaves a truncated file, but the run row stays unfinished and the
current temporary file may be left for the next run to clean up.

## Logging and rotation

- Default level is `info`; set `RUST_LOG` (for example `RUST_LOG=debug`).
- Text logs go to stderr. `--log-json` writes JSON to stdout for collectors.
- `--log-file PATH` adds a daily-rotated file through `tracing-appender`. It keeps one file per
  day and does not compress or delete old files; use logrotate or the container log driver.
- Every run writes `run-<id>.json` under `state.report_dir`. `state.report_days` prunes older
  files after each run; `0` disables pruning.
- The one-line summary is designed to be grepped:

```text
mediaopt run=1 seen=220 changed=165 skipped=0 rejected=55 errors=0 freed=19.15 MiB
```

A `stopped=...` suffix names the budget that ended the run.

## Commands

| Command | Purpose |
| --- | --- |
| `run` | apply the pipeline; `--dry-run` measures without writing, `--no-budget` clears the budgets |
| `plan` | alias for `run --dry-run`, with the budgets cleared |
| `report` | lifetime totals per root, then the most recent `--runs N` runs |
| `tools` | effective config, engine paths and versions, active and remote-managed roots |
| `verify` | re-check a random sample (`--sample`, default 200) of changed files |
| `export` | manifest as gzipped JSON; appends `.json.gz` unless the path ends in `.gz` |
| `import` | merge a snapshot; existing rows win, so it is safe on a live manifest |
| `compact` | delete `error` rows and finished run rows older than 180 days, then `VACUUM` |
| `schedule` | run forever at the configured daily time |

`verify` checks that each sampled path exists, that its size matches `size_after`, and that it
decodes. It exits nonzero when any check fails.

## What to monitor

- Whether a run finished in the last day; catch-up cannot help a stopped service.
- `errors=` in the summary line; the run continues past a single-file error.
- Habitual `stopped=max_runtime` or `stopped=max_files_per_run`: raise the budget or split roots.
- `freed=0` after the first pass is expected; the pilot's second and third runs changed 0 of 220.
- `mediaopt verify` weekly, free space against the reserve, state growth, and engine versions.

## Runbooks
### A run failed

1. Read the exit status and the last log lines. An unreadable config, a missing root and a held
   lock fail before any file is touched.
2. Run `mediaopt tools`. It needs no encoders and shows the effective paths and versions plus a
   warning for every missing required binary.
3. If the error names the lock file, another run is active. 0.1.0 deliberately has no queue flag
   despite the wording of that message: stop the scheduler, let the run finish, then start again.
4. Check free space on every root and on the state filesystem. A root below the reserve is skipped
   with a warning, not treated as fatal.
5. Re-run `mediaopt plan`, then `mediaopt run`; the next scheduled catch-up also retries.
6. A single-file error keeps the original and records `outcome=error`. Since the fast path skips
   non-pending rows with a matching fingerprint, the file is retried only after it changes. To
   force a retry, stop the scheduler, back up the manifest, then run
   `sqlite3 <db> "DELETE FROM files WHERE path='<path>'"`.

### Files were compressed too aggressively

1. Stop the scheduler so nothing else changes.
2. Export the manifest and list the changed rows:

```console
mediaopt export --out /var/lib/mediaopt/pre.json.gz
gzip -dc /var/lib/mediaopt/pre.json.gz | jq -r '.files[] | select(.outcome=="changed") | .path'
```

3. Restore those files from the backup taken before the run. The manifest keeps hashes, not
   original bytes, so the backup is the only source of the original pixels. See
   [rollback](rollback.md).
4. Make the policy more conservative: raise `jpeg.ssimu2_floor` (for example to 80), raise
   `jpeg.reencode_min_quality` so fewer sources are re-encoded, raise `jpeg.fixed_quality` (the
   resize path always uses it), or set the root to `policy = "lossless_only"`.
5. Test on a copy with `plan`, compare the reported `ssimu2` values with the restored originals,
   then re-enable the schedule. Restored files have no COM marker, so they are processed again.

### Disk is filling

1. `du -sh` the state directory. Reports are small but numerous; `state.report_days` controls
   retention and `0` disables pruning entirely.
2. Run `mediaopt compact`. It deletes error rows and finished run rows older than 180 days and
   vacuums the database. It never deletes decision rows for media files, because those make later
   runs cheap.
3. Look for temporary files left by a killed run. A normal run removes them at the start of each
   root, but only directly in the root directory:

```console
find /mnt/media -type f -name '*.mediaopt.*.tmp'
```

Only delete these while no run is active.
4. Check backup and snapshot directories. A snapshot on the same filesystem as the media grows
   with the manifest and is not pruned by the tool.
5. If a root is skipped for free space, the reserve is doing its job. Free space first, or lower
   `limits.free_space_reserve` deliberately.
6. The media tree itself does not grow because of this tool; every replacement is smaller.
