# Rollback

`mediaopt` replaces files in place and does not keep the original bytes. A
rollback is therefore a restore from backup, not an undo. The design keeps that
restore cheap and predictable.

## Preconditions that make rollback work

- **Run after the nightly backup.** The previous state of every file is then in
  the backup set taken before the run. A run that starts before the backup
  finishes can leave files whose pre-run version exists nowhere.
- **Keep per-run budgets.** `limits.max_files_per_run` and
  `limits.max_bytes_per_run` bound how much of the tree one mistake can touch.
  A bad first night is a small restore instead of a full one.
- **Keep the manifest and its export.** The manifest records what changed, when,
  with which settings and at which score. `mediaopt export` turns it into a
  portable snapshot; store that snapshot off the media host.
- **Keep the changed-file list.** The JSON report of every applied run lists the
  top savings, and the manifest export lists every changed path.

## Detect a problem

1. `mediaopt verify --sample 200` re-checks changed files against the manifest.
   It exits nonzero when a file is missing, has a different size, or no longer
   decodes.
2. The run report contains the largest wins with their `ssimu2` values. A score
   close to the floor is a candidate for visual review; a score below the floor
   should not exist and indicates a bug or a corrupted row.
3. A quality complaint from a user is the other signal. It usually names a page
   or an album, which maps to one root.

## Stop further runs

```console
systemctl stop mediaopt
```

or, for the container:

```console
docker stop mediaopt
```

`SIGTERM` is handled by the scheduler: a run in flight is allowed to finish or
hit its budget, then the process exits. Nothing is left half-replaced, because
every file is replaced atomically. If the process is killed instead, the current
temporary file stays behind and is removed at the start of the next run. Give a
container enough stop timeout (`docker stop -t`) if a run is active. The lock is
released when the process exits.

## Identify what changed

Export the manifest and list the changed rows. The snapshot is gzipped JSON with
a top-level `files` array; each row carries `path`, `outcome`, `size_before`,
`size_after`, `action`, `settings` and `ssimu2`.

```console
mediaopt export --out /var/lib/mediaopt/pre-rollback.json.gz
gzip -dc /var/lib/mediaopt/pre-rollback.json.gz \
  | jq -r '.files[] | select(.outcome == "changed") | .path'
```

Restore the whole root, or only the paths that are actually wrong. Restoring a
whole root is simpler and the backup already has it; restoring selected files is
faster when a run was bounded.

## Restore from backup

Use the backup tool's normal restore path. Two properties matter:

- Restore content, ownership, mode and timestamps. The tool preserved them when
  it replaced the file, and the backup should put them back.
- Restore from a backup taken **before** the run you are rolling back, not after
  it.

A restored file is not the file the manifest describes. Its size differs from
the recorded post-swap size, so the fingerprint no longer matches and the next
run sees it as new and processes it again. That is what you want after you have
corrected the configuration; if you do not want it processed, disable the root
or set `policy = "lossless_only"` before the next run.

## `mediaopt verify`

`verify` samples random `changed` rows and reports one line per problem:

| Line | Meaning | Typical cause |
| --- | --- | --- |
| `MISSING` | the recorded path does not exist | media deleted or moved after optimisation |
| `SIZE-CHANGED` | the file size differs from `size_after` | a CMS, sync job or another tool rewrote the file |
| `UNDECODABLE` | the header cannot be read | corruption or a truncated transfer, restore from backup |

The summary line counts checked, ok, missing, changed-size and undecodable
files. A missing file is not always an incident: media gets deleted. A
size-changed or undecodable file needs a look. Run `verify` weekly and after
every restore.

## Repair a lost manifest

If the state database is lost but a snapshot exists, import it:

```console
mediaopt import --input /var/lib/mediaopt/snapshots/state.json.gz
```

The import is additive and existing rows win, so it is safe to run against a
live manifest: it fills gaps and never overwrites newer knowledge. Paths are
imported verbatim; if the media was restored to a different mount point, the
rows will not match and the files will be reconsidered.

With no snapshot at all, recovery relies on the files themselves:

- JPEGs written by the tool carry the COM marker `mediaopt:<version>:<qNN|ll>`.
  They are recognised as already processed and skipped, so they cannot receive a
  second lossy generation.
- The re-encode threshold is 86 and fixed-mode output is q85, so an unmarked
  JPEG is usually below the threshold. A `target` mode output that had to stay
  above it is the exception, which is why the marker and the manifest are the
  primary recovery layers.
- PNG files have no marker. They are reconsidered, which usually means a
  lossless pass with no gain and therefore a rejection, or a palette pass that
  is refused because the file already has a palette.

## Keep a snapshot

Schedule the export after the nightly run and copy it off the host. It is small,
compressed and independent of the SQLite file:

```console
mediaopt export --out /var/lib/mediaopt/snapshots/state.json.gz
```

Keep at least as many snapshots as the backup retention window. A snapshot is
also the cleanest way to carry decisions to a replacement host.

## Rollback checklist

| Step | Action |
| --- | --- |
| Stop | `systemctl stop mediaopt` or `docker stop mediaopt` |
| Record | export the manifest before changing anything |
| Scope | list changed paths from the snapshot; choose file or root restore |
| Restore | restore from the backup taken before the bad run |
| Verify | `mediaopt verify`, then spot-check the restored files |
| Fix | correct the config before the next run: floor, threshold, policy or budget |
| Resume | re-enable the schedule and watch the first report |
