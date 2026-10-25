# Migrating from a cron-driven optimiser

This is a generic checklist for replacing an existing image optimiser that runs
from cron (or a systemd timer) with `mediaopt`. It assumes the old job rewrites
files in place and has no reliable record of what it did.

## 1. Inventory

- List every root the old job touched. Read the crontab, the timer unit and the
  wrapper script; they often disagree.
- Record file count and total bytes per root. Compare this with what `df`
  reports, so a forgotten root or a bind mount shows up before cutover.
- Record the format mix and the stored-quality distribution. A dry run at trace
  level prints one line per JPEG:

```console
RUST_LOG=mediaopt=trace mediaopt plan 2>&1 | grep 'format=Jpeg'
```

- List exclusions: caches, thumbnails, previews, private folders, backup
  folders. They become `exclude` entries or separate roots.
- Record ownership, group and mode of a sample of files, and whether the old job
  ran as root. If it did, decide which account will own replacements now.
- Determine what the old job did to metadata. If it stripped EXIF or ICC, the
  originals are already gone and the new tool can only work with what remains.
- Confirm the backup window and that the backup runs before the optimiser. The
  new schedule must sit after the backup, not before it.
- Note the old tool's state: logs, cache directories, its own manifest or
  database, and any marker it wrote into files. These are cleanup candidates,
  not inputs.

## 2. Pilot on a copy

Never start with the live tree. Copy one representative root:

```console
rsync -aH /home/example/www/static/img/ /srv/pilot/img/
```

Create a pilot config with its own state directory and only the copied root:

```toml
[state]
db = "/srv/pilot/state.db"
report_dir = "/srv/pilot/reports"

[[roots]]
path = "/srv/pilot/img"
enabled = true
```

Then:

```console
mediaopt --config /srv/pilot/config.toml tools
mediaopt --config /srv/pilot/config.toml plan
mediaopt --config /srv/pilot/config.toml run
mediaopt --config /srv/pilot/config.toml verify --sample 200
```

- Review the top savings in the run output and open a sample of the changed
  files at 1:1.
- Compare the reported `ssimu2` values with `jpeg.ssimu2_floor`. Files close to
  the floor are the ones most likely to be noticed.
- Run the pipeline a second and third time. Expect 0 changes. The measured pilot
  changed 0 of 220 files on both runs.
- Test the rollback path on the copy before touching production.

## 3. Budgets per night

The first production runs should be deliberately small.

- Set `limits.max_files_per_run` and `limits.max_bytes_per_run` to a fraction of
  the tree. The byte budget counts the input size of replaced files and the run
  stops cleanly between files.
- Keep `limits.max_runtime` short enough that the run finishes inside the
  maintenance window, and well before the tree is busy again.
- Start with one root, then add the rest. Each night's run builds on the
  manifest from the previous one.
- Leave `limits.free_space_reserve` at the default until the filesystem's real
  headroom is known.

## 4. Deploy order

1. Confirm the last backup is complete and restorable.
2. Install the tool and the encoders; run `mediaopt tools` and record the
   versions.
3. Create `/etc/mediaopt/config.toml` and the state directory.
4. Disable the old cron entry or timer, but keep the old binary and its data for
   the rollback window. Do not run the old and new tools on the same tree at the
   same time: each would invalidate the other's assumptions.
5. Run `mediaopt plan` with the production config. Check the roots, the counts
   and the projected savings.
6. Run one bounded `mediaopt run` off-peak and watch the log and the report.
7. Run `mediaopt verify`. Sample the changed files visually.
8. Enable `mediaopt schedule` under systemd or as a container.
9. After two or three clean nights, remove the old cron entry and retire the old
   tool.

During the rollback window, keep both the old tool and the last backup set. The
new tool records what it changed, but it does not keep the original bytes.

## 5. Cleanup after cutover

- Remove the old crontab line or timer unit and any wrapper script it called.
- Keep the old tool's state and logs until one full backup rotation has passed,
  then delete them.
- If the old tool wrote a marker into files (for example an "optimised" comment),
  do not spend a re-encode removing it. `mediaopt` records but ignores the legacy
  marker on purpose.
- Remove stale lock files and temporary directories the old tool left behind,
  after checking that no old process is still running.
- Schedule a manifest export (`mediaopt export --out ...`) and store the
  snapshot off the media host. It is the fastest way to repair a lost manifest.
- Add monitoring for the daily summary line, `mediaopt verify` and free space.
- Write down the new schedule, the root list, the budgets and the rollback
  procedure in the host's runbook.

## Cutover checklist

| Step | Done when |
| --- | --- |
| Roots inventoried | every path from cron and the wrapper is classified |
| Backup verified | a restore of one file has been tested |
| Pilot clean | third run reports `changed=0` |
| Engines recorded | `mediaopt tools` output saved with the deploy |
| Old schedule disabled | no old optimiser will touch the tree |
| First bounded run | report reviewed and a sample checked visually |
| Verification | `mediaopt verify` exits zero |
| Schedule enabled | next run appears in the log within one day |
| Old tool retired | binary, state and cron entry removed after the rollback window |
| Snapshot stored | at least one `export` exists off the media host |
