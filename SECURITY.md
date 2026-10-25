# Security Policy

## Supported versions

| Version | Supported |
| --- | --- |
| 0.1.x | yes, the latest patch release |
| none earlier | 0.1.0 is the first release |

Before 1.0 only the newest release receives fixes. Upgrade before reporting a
problem that may already be fixed.

## Reporting a vulnerability

Report privately through the repository's GitHub Security Advisories page:

<https://github.com/HansVanEijsden/media-optimizer/security/advisories>

Do not open a public issue for a vulnerability. Include:

- the `mediaopt --version` output and the operating system;
- the relevant configuration with paths and any secrets removed;
- a minimal reproduction, ideally with a synthetic image;
- what you expected and what happened.

Do not include production media, hostnames, IP addresses, customer names,
manifest exports or log files that contain real paths. A sanitised excerpt is
enough. Reports are handled on a best-effort basis; there is no bug bounty.

## Trust model

`mediaopt` is a local command line tool, not a service.

- It opens no socket and makes no network requests. It does not fetch, upload or
  phone home, and it has no telemetry or auto-update.
- It only reads and writes the paths it is configured to touch: the roots in the
  config, the manifest database, the report directory and an optional log file.
- It spawns encoder programs directly, without a shell. The command line is
  built from configuration values and file paths, never from file contents.
- The encoder binaries are taken from `[engine]`, an environment variable or
  `PATH`, in that order. A malicious binary earlier in `PATH` runs with the
  tool's privileges, so treat the tool's `PATH` as trusted.
- File names are never executed or interpreted as a format. The format comes
  from the file's magic bytes.
- Directory symlinks are not followed during the walk. A root can still point at
  any directory the running account can read, because the config is trusted
  input.
- Runs should use a dedicated unprivileged account. The tool preserves owner,
  group and mode when it replaces a file and attempts to restore them; when run
  as root it can therefore `chown` a replacement to another account, which is
  intentional but worth knowing.
- The configuration and the manifest are trusted state. Anyone who can edit them
  can redirect the tool at other directories or suppress its safeguards, so
  protect `/etc/mediaopt` and the state directory with normal file permissions.

## What is in scope

- A bug that causes data loss, replacement with a broken file, or replacement
  after a failed verification.
- Path traversal or unexpected file access outside the configured roots.
- A symlink or rename race that escapes the atomic replacement contract.
- A crash or denial of service triggered by a crafted image in the header
  parsers or the encoders.

## What is out of scope

- Vulnerabilities in libvips, mozjpeg, pngquant, `ssimulacra2` or oxipng. Report
  those upstream; mention them here if the tool's defaults make them reachable.
- A local attacker who can already write to the media tree, the state directory
  or the configuration.
- The information content of reports and the manifest. They contain absolute
  paths and the host name by design; treat them as internal files and do not
  publish them.
