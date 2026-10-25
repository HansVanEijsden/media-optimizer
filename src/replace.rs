//! Atomic file replacement with full metadata preservation.
//!
//! The contract: a media file is either the old file or the new file, never a
//! truncated one, and after the swap it still has the same owner, group, mode and
//! modification time. WordPress, nginx and imgproxy all read these trees while we
//! work, so anything less would be a bug waiting for a busy afternoon.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct FileMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: SystemTime,
    pub atime: SystemTime,
}

pub fn read_meta(path: &Path) -> io::Result<FileMeta> {
    let md = std::fs::metadata(path)?;
    Ok(FileMeta {
        mode: md.mode() & 0o7777,
        uid: md.uid(),
        gid: md.gid(),
        mtime: md.modified()?,
        atime: md.accessed().unwrap_or_else(|_| SystemTime::now()),
    })
}

/// A temporary path in the system temporary directory, for a dry run: it never
/// replaces anything, and this lets a `plan` run work against a read-only mount.
pub fn scratch_path(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(".mediaopt.{}.{n}.{name}.tmp", std::process::id()))
}

/// A temporary path next to `dst`, so the final rename stays inside one
/// filesystem and therefore atomic.
pub fn temp_path(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dst.with_file_name(format!(".{name}.mediaopt.{}.{n}.tmp", std::process::id()))
}

pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".mediaopt.") && name.ends_with(".tmp")
}

/// Apply mode, ownership and timestamps to a path.
pub fn apply_meta(path: &Path, meta: &FileMeta, warn: &mut dyn FnMut(&str)) {
    if let Err(e) = std::fs::set_permissions(
        path,
        std::os::unix::fs::PermissionsExt::from_mode(meta.mode),
    ) {
        warn(&format!("chmod {}: {e}", path.display()));
    }
    // SAFETY: plain libc calls on a NUL-terminated path we own.
    let c_path = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => {
            warn(&format!("path contains NUL: {}", path.display()));
            return;
        }
    };
    let rc = unsafe { libc::chown(c_path.as_ptr(), meta.uid, meta.gid) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // EPERM is expected when running unprivileged on a tree owned by someone
        // else; the caller decides whether that is fatal.
        warn(&format!(
            "chown {} -> {}:{}: {err}",
            path.display(),
            meta.uid,
            meta.gid
        ));
    }
    if let Err(e) = filetime::set_file_times(
        path,
        filetime::FileTime::from_system_time(meta.atime),
        filetime::FileTime::from_system_time(meta.mtime),
    ) {
        warn(&format!("utimes {}: {e}", path.display()));
    }
}

/// Write `bytes` to a temporary file next to `dst`, fsync it, then atomically
/// rename it over `dst` and fsync the directory.
pub fn replace_with_bytes(dst: &Path, bytes: &[u8], meta: &FileMeta) -> Result<()> {
    let temp = temp_path(dst);
    let result = (|| -> Result<()> {
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .with_context(|| format!("creating {}", temp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing {}", temp.display()))?;
            f.sync_all()
                .with_context(|| format!("fsync {}", temp.display()))?;
        }
        finish_replace(&temp, dst, meta)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Rename an already-written temporary file over `dst`.
pub fn finish_replace(temp: &Path, dst: &Path, meta: &FileMeta) -> Result<()> {
    let mut warnings = Vec::new();
    apply_meta(temp, meta, &mut |w| warnings.push(w.to_string()));
    std::fs::rename(temp, dst)
        .with_context(|| format!("renaming {} -> {}", temp.display(), dst.display()))?;
    sync_parent(dst)?;
    for w in warnings {
        tracing::warn!("{w}");
    }
    Ok(())
}

/// fsync the directory so the rename itself is durable.
pub fn sync_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = File::open(parent).with_context(|| format!("opening dir {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("fsync dir {}", parent.display()))?;
    Ok(())
}

/// Install a file that lives somewhere else (a staging area, another mount).
///
/// `rename()` cannot cross a mount boundary: inside a container the staging
/// directory and the media tree are separate bind mounts, and the kernel returns
/// EXDEV even when both sit on the same disk. So the file is copied to a
/// temporary name *beside the target* first, which is the same mount, and only
/// then renamed into place.
pub fn install_from(staged: &Path, target: &Path, meta: &FileMeta) -> Result<()> {
    let temp = temp_path(target);
    let result = (|| -> Result<()> {
        std::fs::copy(staged, &temp)
            .with_context(|| format!("copying {} to {}", staged.display(), temp.display()))?;
        File::open(&temp)
            .and_then(|f| f.sync_all())
            .with_context(|| format!("fsync {}", temp.display()))?;
        finish_replace(&temp, target, meta)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Streaming SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// SHA-256 of an in-memory buffer.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Nanoseconds since the epoch, as stored in the manifest.
pub fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(md.mtime_nsec())
}

/// Delete temporary files an earlier run left behind, recursively.
///
/// Only files older than `min_age` are touched: a temp younger than that may
/// belong to a run that is still working, and deleting it would corrupt that
/// encode. The first version of this function looked only at the top of the root
/// and so never found anything in a year/month tree, which is why a leak of tens
/// of thousands of files went unnoticed.
pub fn clean_stale_temps(dir: &Path, min_age: std::time::Duration) -> usize {
    let mut removed = 0;
    let now = std::time::SystemTime::now();
    // No filter_entry here: it would drop the files as well as the directories.
    let walker = walkdir::WalkDir::new(dir).follow_links(false);
    for entry in walker.into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_temp_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age < min_age {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_paths_live_beside_the_target_and_are_recognisable() {
        let dst = Path::new("/home/example/www/2024/05/photo.jpg");
        let temp = temp_path(dst);
        assert_eq!(temp.parent(), dst.parent());
        assert!(is_temp_name(&temp.file_name().unwrap().to_string_lossy()));
        let other = temp_path(dst);
        assert_ne!(temp, other, "two workers must not collide");
        assert!(!is_temp_name("photo.jpg"));
    }

    #[test]
    fn scratch_paths_leave_the_media_tree_alone() {
        let dst = Path::new("/home/example/www/2024/05/photo.jpg");
        let scratch = scratch_path(dst);
        assert_ne!(scratch.parent(), dst.parent());
        assert_eq!(scratch.parent(), Some(std::env::temp_dir().as_path()));
        assert!(is_temp_name(
            &scratch.file_name().unwrap().to_string_lossy()
        ));
    }

    #[test]
    fn replace_preserves_mode_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("photo.jpg");
        std::fs::write(&dst, b"original").unwrap();
        let mut perms = std::fs::metadata(&dst).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o640);
        std::fs::set_permissions(&dst, perms).unwrap();
        let meta = read_meta(&dst).unwrap();

        replace_with_bytes(&dst, b"replacement", &meta).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"replacement");
        let after = std::fs::metadata(&dst).unwrap();
        assert_eq!(after.mode() & 0o777, 0o640);
        assert_eq!(
            after.modified().unwrap(),
            meta.mtime,
            "mtime must be preserved so caches and rsync see no change"
        );
        // No temporary files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| is_temp_name(&e.file_name().to_string_lossy()))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn hashing_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, b"hello").unwrap();
        let file_hash = sha256_file(&p).unwrap();
        assert_eq!(file_hash, sha256_bytes(b"hello"));
        assert_eq!(
            file_hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn install_from_another_directory_keeps_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        let target = dir.path().join("photo.jpg");
        std::fs::write(&target, b"original-with-a-longer-body").unwrap();
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o640);
        std::fs::set_permissions(&target, perms).unwrap();
        let meta = read_meta(&target).unwrap();

        let staged = stage.path().join("7");
        std::fs::write(&staged, b"smaller").unwrap();
        install_from(&staged, &target, &meta).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"smaller");
        let after = std::fs::metadata(&target).unwrap();
        assert_eq!(after.mode() & 0o777, 0o640);
        assert_eq!(after.modified().unwrap(), meta.mtime);
        // No temporary files left next to the target.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| is_temp_name(&e.file_name().to_string_lossy()))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn stale_temps_are_cleaned_recursively_but_young_ones_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("2024/05");
        std::fs::create_dir_all(&nested).unwrap();
        let dst = nested.join("photo.jpg");
        std::fs::write(&dst, b"real").unwrap();
        let old_temp = temp_path(&dst);
        std::fs::write(&old_temp, b"junk").unwrap();
        // The nested tree is where the old version of this function never looked.
        assert_eq!(clean_stale_temps(dir.path(), std::time::Duration::ZERO), 1);
        assert!(dst.exists());
        assert!(!old_temp.exists());

        let young = temp_path(&dst);
        std::fs::write(&young, b"in flight").unwrap();
        assert_eq!(
            clean_stale_temps(dir.path(), std::time::Duration::from_secs(1800)),
            0,
            "a temp file a running encode may still be using must survive"
        );
        assert!(young.exists());
    }
}
