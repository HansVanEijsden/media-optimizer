#!/usr/bin/env bash
#
# check-no-secrets.sh - refuse to publish hostnames, addresses and tokens.
#
# Two layers, deliberately separated:
#
#  1. Generic patterns live in this file and are always checked: e-mail
#     addresses, IPv4 addresses, private key headers and long hex tokens.
#  2. Project-specific names (customers, hosts, private domains) must NEVER be
#     committed, because a blocklist of the names you are protecting is itself
#     the leak. Put them in a file outside the repository and point
#     $MEDIAOPT_BLOCKLIST at it. The default location is
#     ~/.config/mediaopt/blocklist.txt. Without it, only layer 1 runs.
#
# Usage:
#   ./scripts/check-no-secrets.sh            # every tracked file
#   ./scripts/check-no-secrets.sh --all      # tracked files plus untracked
#
# Exit status: 0 clean, 1 findings, 2 cannot run. Nothing is modified.
set -uo pipefail

include_untracked=0
for arg in "$@"; do
  case "$arg" in
    --all) include_untracked=1 ;;
    -h|--help) sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "not inside a git repository" >&2
  exit 2
}
cd "$repo_root"

self="$repo_root/scripts/check-no-secrets.sh"
files=$(git ls-files)
if [ "$include_untracked" = 1 ]; then
  files=$(printf '%s\n%s\n' "$files" "$(git ls-files --others --exclude-standard)")
fi

# Layer 1. Each entry is "label|extended-regular-expression".
patterns=(
  'private key header|-----BEGIN [A-Z ]*PRIVATE KEY-----'
  'e-mail address|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
  'IPv4 address|([0-9]{1,3}\.){3}[0-9]{1,3}'
  'long hex token|[0-9a-fA-F]{40,}'
  'AWS access key|AKIA[0-9A-Z]{16}'
  'GitHub token|gh[pousr]_[A-Za-z0-9]{30,}'
  'Slack token|xox[baprs]-[A-Za-z0-9-]{10,}'
)
# Occurrences that are legitimate in a public repository.
allow_regex='example\.(com|org|net)|127\.0\.0\.1|0\.0\.0\.0|users\.noreply\.github\.com|noreply@|@vips|@[12]x|libjpeg|libpng|@rpath|apache\.org|debian\.org|adobe\.com|github\.com|ghcr\.io|sha256|blake2|md5|@types|@1\.0|@gradle'

# Files whose long hex strings are checksums, not secrets.
skip_hex='Cargo\.lock'
# Reviewed non-secret constants: the SHA-256 of "hello" used by a unit test.
allow_hex='2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824'

# Layer 2. Project-specific names, read from outside the repository.
blocklist_file=${MEDIAOPT_BLOCKLIST:-$HOME/.config/mediaopt/blocklist.txt}
blocklist=()
if [ -r "$blocklist_file" ]; then
  while IFS= read -r line; do
    line=${line%%#*}
    line=$(printf '%s' "$line" | tr -d '[:space:]')
    [ -n "$line" ] && blocklist+=("$line")
  done < "$blocklist_file"
fi

hits=0
report() { printf '%s:%s: %s\n' "$1" "$2" "$3"; hits=$((hits + 1)); }

for entry in "${patterns[@]}"; do
  label=${entry%%|*}
  regex=${entry#*|}
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file=${hit%%:*}
    rest=${hit#*:}
    line=${rest%%:*}
    body=${rest#*:}
    [ "$file" = "$self" ] && continue
    if [ "$label" = "long hex token" ]; then
      printf '%s\n' "$file" | grep -qE "$skip_hex" && continue
      printf '%s\n' "$body" | grep -qF "$allow_hex" && continue
    fi
    report "$file" "$line" "$label"
  done < <(printf '%s\n' "$files" | xargs grep -nIE "$regex" 2>/dev/null | grep -ivE "$allow_regex")
done

if [ ${#blocklist[@]} -gt 0 ]; then
  for term in "${blocklist[@]}"; do
    while IFS= read -r hit; do
      [ -n "$hit" ] || continue
      file=${hit%%:*}
      rest=${hit#*:}
      line=${rest%%:*}
      [ "$file" = "$self" ] && continue
      report "$file" "$line" "blocked term"
    done < <(printf '%s\n' "$files" | xargs grep -niF -- "$term" 2>/dev/null)
  done
else
  echo "note: no blocklist at $blocklist_file, only the generic checks ran" >&2
fi

count=$(printf '%s\n' "$files" | grep -c . || true)
if [ "$hits" -gt 0 ]; then
  echo
  echo "check-no-secrets: $hits problem(s) in $count tracked file(s)"
  exit 1
fi
echo "check-no-secrets: OK - $count tracked file(s), no addresses or secrets"
