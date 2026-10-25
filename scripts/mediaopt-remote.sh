#!/usr/bin/env bash
#
# mediaopt-remote.sh - let this machine do the encoding for a remote host.
#
# The server keeps the manifest and the media; this machine does the work. That
# matters because the expensive part of mediaopt is the encoder, not the disk: a
# two-core server needs days for a million-file backlog, a laptop does it in
# hours.
#
#   ./mediaopt-remote.sh --host web-a                       # everything
#   ./mediaopt-remote.sh --host web-a --limit 200 --batch 100   # a taste
#   ./mediaopt-remote.sh --host web-a --root /srv/example/img   # one root
#
# What it does, per batch:
#   1. the server lists the files that still need work (`scan`, no writes)
#   2. this machine downloads the batch over ssh
#   3. this machine encodes it with its own engines, in parallel
#   4. the batch goes back into a staging directory on the server
#   5. the server re-verifies every file and only then replaces it (`apply-staged`)
#
# The server side runs inside the container that is already deployed there, so
# nothing has to be installed on it. The scheduler is stopped for the duration
# and started again at the end, so the two never touch the same file at once.
#
# Requirements on this machine: the mediaopt binary (see --binary), libvips,
# mozjpeg jpegtran, pngquant and ssimulacra2, all four on PATH or in $VIPS_BIN,
# $JPEGTRAN_BIN, $PNGQUANT_BIN and $SSIMULACRA2_BIN. Requirements on the server:
# ssh access as root and the mediaopt container.
#
set -Eeuo pipefail

host=""
root=""
batch=2000
jobs=10
limit=0
config=""
binary="mediaopt"
# The staging directory has to be reachable from both sides: inside the container
# (where apply-staged runs) and on the host (where tar unpacks). It lives inside
# the bind mount that is the state directory, so both stay on the same filesystem
# as the media and the final rename stays atomic.
stage_container="/var/lib/mediaopt/stage"
container="mediaopt"
image="mediaopt:0.1.0"
work=""
keep_work=0

usage() {
  sed -n '3,40p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --host) host=${2:?}; shift 2 ;;
    --root) root=${2:?}; shift 2 ;;
    --batch) batch=${2:?}; shift 2 ;;
    --jobs) jobs=${2:?}; shift 2 ;;
    --limit) limit=${2:?}; shift 2 ;;
    --config) config=${2:?}; shift 2 ;;
    --binary) binary=${2:?}; shift 2 ;;
    --container) container=${2:?}; shift 2 ;;
    --image) image=${2:?}; shift 2 ;;
    --work) work=${2:?}; shift 2 ;;
    --keep-work) keep_work=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 1 ;;
  esac
done

[ -n "$host" ] || { echo "--host is required" >&2; usage 1; }
[ -x "$binary" ] || command -v "$binary" >/dev/null || {
  echo "mediaopt binary not found: $binary" >&2; exit 1; }

work=${work:-$(mktemp -d -t mediaopt-remote)}
mkdir -p "$work/in" "$work/out"
trap 'cleanup' EXIT

cleanup() {
  local status=$?
  if [ "$keep_work" = 0 ]; then rm -rf "$work/in" "$work/out"; fi
  # Never leave the scheduler stopped because this script died.
  ssh -o BatchMode=yes "$host" "docker start $container >/dev/null 2>&1 || true" || true
  exit "$status"
}

say() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# The mounts of the running container define what the one-shot commands see:
# the config, the state directory and the media roots. Reusing them means this
# script cannot drift from the deployed configuration.
say "reading the container mounts on $host"
mountpairs=$(ssh -o BatchMode=yes "$host" \
  "docker inspect $container --format '{{range .Mounts}}{{.Destination}}={{.Source}} {{end}}'")
[ -n "$mountpairs" ] || {
  echo "could not read the mounts of $container on $host" >&2; exit 1; }

mounts=""
state_host=""
for pair in $mountpairs; do
  dest=${pair%%=*}
  src=${pair#*=}
  mounts="$mounts -v $src:$dest"
  [ "$dest" = "/var/lib/mediaopt" ] && state_host=$src
done
[ -n "$state_host" ] || {
  echo "could not find the state directory mount of $container on $host" >&2; exit 1; }
stage_host="$state_host/stage"

# Run a one-shot mediaopt command on the server, in the same sandbox.
remote_mediaopt() {
  # shellcheck disable=SC2086
  ssh -o BatchMode=yes "$host" \
    "docker run --rm -i --user 0:0 --cpus=1 --read-only --tmpfs /tmp:size=256m \
       --security-opt no-new-privileges:true --cap-drop ALL \
       --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER \
       $mounts $image $*"
}

say "stopping the scheduler on $host so the two cannot race"
ssh -o BatchMode=yes "$host" "docker stop $container >/dev/null 2>&1 || true"

say "scanning on $host (this walks the tree, no writes)"
scan_args="scan"
[ -n "$root" ] && scan_args="$scan_args --root $root"
[ "$limit" -gt 0 ] && scan_args="$scan_args --limit $limit"
remote_mediaopt "$scan_args" > "$work/candidates.tsv"

total=$(wc -l < "$work/candidates.tsv" | tr -d ' ')
say "$total file(s) need work"
[ "$total" -gt 0 ] || { say "nothing to do"; exit 0; }

# One split instead of slicing the whole list per batch.
split -l "$batch" "$work/candidates.tsv" "$work/batch-"
batch_files=$(ls "$work"/batch-* | sort)

done_count=0
batch_no=0
while :; do
  batch_no=$((batch_no + 1))
  batch_file=$(printf '%s\n' "$batch_files" | sed -n "${batch_no}p")
  [ -n "$batch_file" ] || break

  cp "$batch_file" "$work/batch.tsv"
  n=$(wc -l < "$work/batch.tsv" | tr -d ' ')
  say "batch $batch_no: $n file(s)"

  t0=$(date +%s)
  # Download: tar keeps the tree, so the local copy lands at the same relative
  # path it has on the server and `encode-batch` can find it.
  cut -f4 "$work/batch.tsv" | sed 's#^/##' > "$work/paths.txt"
  rm -rf "$work/in" "$work/out"; mkdir -p "$work/in" "$work/out"
  COPYFILE_DISABLE=1 ssh -o BatchMode=yes "$host" "tar -cf - -C / -T -" < "$work/paths.txt" \
    | tar --no-xattrs -xf - -C "$work/in" 2>/dev/null

  t1=$(date +%s)
  # Encode here.
  "$binary" --config "$config" encode-batch \
      --list "$work/batch.tsv" \
      --input "$work/in" \
      --output "$work/out" \
      --result "$work/results.tsv" \
      --jobs "$jobs"

  t2=$(date +%s)
  say "  download $(( t1 - t0 ))s, encode $(( t2 - t1 ))s"
  # Upload the results and let the server verify and install them.
  if [ -n "$(ls -A "$work/out" 2>/dev/null)" ]; then
    ssh -o BatchMode=yes "$host" "rm -rf $stage_host && mkdir -p $stage_host"
    COPYFILE_DISABLE=1 tar --no-xattrs -cf - -C "$work/out" . \
      | ssh -o BatchMode=yes "$host" "tar -xf - -C $stage_host"
    remote_mediaopt "apply-staged --input $stage_container --result -" < "$work/results.tsv" \
      | sed 's/^/  /' >&2
  else
    say "  nothing worth applying in this batch"
  fi

  done_count=$((done_count + n))
  say "  progress: $done_count / $total"
done

say "done: $done_count file(s) processed"
