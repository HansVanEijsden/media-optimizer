//! End-to-end tests that use the real encoders.
//!
//! They skip themselves when the engine binaries are not installed, so `cargo
//! test` stays green on a bare CI runner while still exercising the whole
//! pipeline on a machine (or container) that has libvips.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use mediaopt::config::Config;
use mediaopt::engine::Tools;
use mediaopt::manifest::Manifest;
use mediaopt::run::{self, RunOptions};

/// These tests drive the real encoders and share rayon's global pool, so run one
/// at a time inside the test binary regardless of --test-threads.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn tool(name: &str, env: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

struct Engines {
    vips: PathBuf,
    jpegtran: Option<PathBuf>,
}

fn engines() -> Option<Engines> {
    let vips = tool("vips", "VIPS_BIN")?;
    // A binary that exists but cannot run is worse than none: probe it.
    let works = Command::new(&vips)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !works {
        eprintln!("skipping: vips at {} does not run", vips.display());
        return None;
    }
    Some(Engines {
        vips,
        jpegtran: tool("jpegtran", "JPEGTRAN_BIN"),
    })
}

/// Make a high-quality JPEG of a colour chart. A chart, not noise: random noise
/// is the pathological case for any perceptual metric and would be (correctly)
/// rejected by the quality floor, which is not what these tests are about.
fn make_jpeg(vips: &Path, path: &Path, width: u32, height: u32, quality: u32) {
    let source = path.with_extension("v");
    let status = Command::new(vips)
        .arg("xyz")
        .arg(&source)
        .arg(width.to_string())
        .arg(height.to_string())
        .status()
        .expect("vips xyz");
    assert!(status.success(), "could not generate a test image");
    let status = Command::new(vips)
        .arg("jpegsave")
        .arg(&source)
        .arg(path)
        .arg("--Q")
        .arg(quality.to_string())
        .arg("--keep")
        .arg("all")
        .status()
        .expect("vips jpegsave");
    assert!(status.success(), "could not write the test JPEG");
    let _ = std::fs::remove_file(&source);
}

fn write_config(dir: &Path, root: &Path, extra: &str, engines: &Engines) -> PathBuf {
    let config_path = dir.join("config.toml");
    let mut text = format!(
        r#"
[jpeg]
quality_mode = "fixed"
fixed_quality = 82
ssimu2_floor = 60.0
reencode_min_quality = 86
max_dimension = 2048
min_gain_percent = 2.0
lossless_pass = true
min_gain_percent_lossless = 0.5

[png]
mode = "lossless"
lossless_effort = 1

[limits]
jobs = 2
max_runtime = "60s"
min_file_age = "0s"
free_space_reserve = "1MiB"
min_file_size = 1024
{extra}

[state]
db = "{}"
report_dir = "{}"

[engine]
vips = "{}"
"#,
        dir.join("state.db").display(),
        dir.join("reports").display(),
        engines.vips.display()
    );
    if let Some(jpegtran) = &engines.jpegtran {
        text.push_str(&format!("jpegtran = \"{}\"\n", jpegtran.display()));
    }
    text.push_str(&format!(
        "\n[[roots]]\npath = \"{}\"\nenabled = true\n",
        root.display()
    ));
    std::fs::write(&config_path, text).expect("write config");
    config_path
}

fn load(config_path: &Path, dir: &Path) -> Config {
    let mut cfg = Config::load(config_path).expect("config must load");
    // Keep the manifest and reports inside the test directory.
    cfg.state.db = dir.join("state.db");
    cfg.state.report_dir = dir.join("reports");
    cfg
}

fn stat_parts(path: &Path) -> (u64, u64, i64) {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).expect("stat");
    (md.len(), md.mode() as u64 & 0o777, md.mtime())
}

#[test]
fn reencodes_high_quality_jpeg_and_is_idempotent() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("photo.jpg");
    make_jpeg(&engines.vips, &image, 900, 600, 98);
    let original_size = std::fs::metadata(&image).unwrap().len();
    let (_, original_mode, original_mtime) = stat_parts(&image);

    let config_path = write_config(dir.path(), &root, "", &engines);
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let first = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        first.totals.files_changed, 1,
        "the q98 JPEG must be re-encoded"
    );
    assert!(first.totals.bytes_saved() > 0);
    assert_eq!(first.totals.errors, 0);

    let (new_size, new_mode, new_mtime) = stat_parts(&image);
    assert!(new_size < original_size, "output must be smaller");
    assert_eq!(new_mode, original_mode, "permissions must be preserved");
    assert_eq!(new_mtime, original_mtime, "mtime must be preserved");

    // The output must still decode at the original dimensions.
    let header = mediaopt::engine::header_field(&tools, &image, "width").unwrap();
    assert_eq!(header.as_deref(), Some("900"));

    // ... and carry our marker so a lost manifest is recoverable.
    let probed = mediaopt::probe::probe(&image).unwrap();
    let info = probed.jpeg.expect("still a JPEG");
    assert!(
        info.has_tool_marker(),
        "output must carry the mediaopt marker"
    );

    // Second and third run: nothing to do.
    let second = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        second.totals.files_changed, 0,
        "the second run must be a no-op"
    );
    let third = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        third.totals.files_changed, 0,
        "the third run must be a no-op"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("photo.jpg");
    make_jpeg(&engines.vips, &image, 700, 500, 97);
    let before = std::fs::read(&image).unwrap();
    let (size_before, _, mtime_before) = stat_parts(&image);

    let config_path = write_config(dir.path(), &root, "", &engines);
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: true }, "dry-run").unwrap();
    assert!(report.dry_run);
    assert_eq!(report.totals.files_changed, 0);
    assert_eq!(
        report.per_root[0].planned_change, 1,
        "the projection must see the work"
    );
    assert!(
        report.per_root[0].planned_bytes_before > report.per_root[0].planned_bytes_after,
        "the projection must estimate a saving"
    );

    let after = std::fs::read(&image).unwrap();
    assert_eq!(before, after, "a dry run must not modify the file");
    let (size_after, _, mtime_after) = stat_parts(&image);
    assert_eq!(size_before, size_after);
    assert_eq!(mtime_before, mtime_after);
    // No manifest rows: a dry run must not poison the fast path either.
    let manifest = Manifest::open(&dir.path().join("state.db")).unwrap();
    assert_eq!(manifest.count_files().unwrap(), 0);
}

#[test]
fn budget_stops_the_run_early() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..4 {
        make_jpeg(
            &engines.vips,
            &root.join(format!("photo{i}.jpg")),
            500,
            400,
            98,
        );
    }
    let config_path = write_config(dir.path(), &root, "max_files_per_run = 2", &engines);
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        report.stopped,
        Some("max_files_per_run"),
        "the budget must stop the run"
    );
    assert!(
        report.totals.files_changed <= 2,
        "the budget must bound the work, changed {}",
        report.totals.files_changed
    );
    let attempted = report.totals.files_changed + report.totals.files_rejected;
    assert_eq!(
        attempted, 2,
        "exactly the budgeted number of files is encoded"
    );
}

#[test]
fn budget_is_per_run_not_per_root() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    for i in 0..2 {
        make_jpeg(
            &engines.vips,
            &first.join(format!("a{i}.jpg")),
            500,
            400,
            98,
        );
        make_jpeg(
            &engines.vips,
            &second.join(format!("b{i}.jpg")),
            500,
            400,
            98,
        );
    }

    let config_path = dir.path().join("config.toml");
    let mut text = format!(
        r#"
[jpeg]
quality_mode = "fixed"
fixed_quality = 82
ssimu2_floor = 60.0
reencode_min_quality = 86
max_dimension = 2048
min_gain_percent = 2.0
lossless_pass = true

[limits]
jobs = 2
max_runtime = "600s"
max_files_per_run = 3
min_file_age = "0s"
free_space_reserve = "1MiB"
min_file_size = 1024

[state]
db = "{}"
report_dir = "{}"

[engine]
vips = "{}"

[[roots]]
path = "{}"
enabled = true

[[roots]]
path = "{}"
enabled = true
"#,
        dir.path().join("state.db").display(),
        dir.path().join("reports").display(),
        engines.vips.display(),
        first.display(),
        second.display()
    );
    if let Some(jpegtran) = &engines.jpegtran {
        text = text.replace(
            "[engine]\n",
            &format!("[engine]\njpegtran = \"{}\"\n", jpegtran.display()),
        );
    }
    std::fs::write(&config_path, text).unwrap();
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();

    let attempts = |i: usize| {
        report.per_root[i].changed + report.per_root[i].rejected + report.per_root[i].errors
    };
    assert_eq!(report.per_root.len(), 2, "both roots must be reported");
    assert_eq!(
        report.stopped,
        Some("max_files_per_run"),
        "the run budget must stop the whole run"
    );
    assert_eq!(attempts(0), 2, "the first root fits inside the budget");
    assert_eq!(
        attempts(1),
        1,
        "the second root gets the run's remaining allowance, not a fresh one"
    );
    assert_eq!(
        attempts(0) + attempts(1),
        3,
        "max_files_per_run counts attempts per run, across roots"
    );
}

#[test]
fn lossless_only_root_never_degrades_quality() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let Some(_) = &engines.jpegtran else {
        eprintln!("skipping: jpegtran not available");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("keepsake.jpg");
    make_jpeg(&engines.vips, &image, 600, 400, 96);

    let config_path = dir.path().join("config.toml");
    let text = format!(
        r#"
[jpeg]
quality_mode = "fixed"
fixed_quality = 70
ssimu2_floor = 60.0
reencode_min_quality = 0
max_dimension = 0
lossless_pass = true

[limits]
min_file_age = "0s"
min_file_size = 1024

[state]
db = "{}"
report_dir = "{}"

[engine]
vips = "{}"
jpegtran = "{}"

[[roots]]
path = "{}"
enabled = true
policy = "lossless_only"
"#,
        dir.path().join("state.db").display(),
        dir.path().join("reports").display(),
        engines.vips.display(),
        engines.jpegtran.as_ref().unwrap().display(),
        root.display()
    );
    std::fs::write(&config_path, text).unwrap();
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(report.totals.errors, 0);
    // Whatever happened, the stored quality must not have dropped to the
    // configured fixed_quality of 70.
    let probed = mediaopt::probe::probe(&image).unwrap();
    let info = probed.jpeg.expect("still a JPEG");
    assert!(
        info.quality >= 90,
        "a lossless_only root must never re-encode, stored quality is now {}",
        info.quality
    );
}

#[test]
fn manifest_records_survive_a_new_inode_and_mtime() {
    let _serial = serial();
    let Some(engines) = engines() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("photo.jpg");
    make_jpeg(&engines.vips, &image, 640, 480, 98);

    let config_path = write_config(dir.path(), &root, "", &engines);
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);
    run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    let changed = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(changed.totals.files_changed, 0);

    // Simulate a restore onto new hardware: same bytes, new inode, new mtime.
    let copy = root.join("photo.restored.jpg");
    std::fs::copy(&image, &copy).unwrap();
    std::fs::remove_file(&image).unwrap();
    std::fs::rename(&copy, &image).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    filetime::set_file_mtime(&image, filetime::FileTime::now()).unwrap();

    let after_restore = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        after_restore.totals.files_changed, 0,
        "a restored file with our marker must be recognised without the manifest"
    );
}

#[test]
fn changing_the_config_re_evaluates_settled_files() {
    let Some(engines) = engines() else { return };
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("lean.jpg");
    // A file at q82: below the re-encode threshold, so it is only interesting for
    // the lossless pass.
    make_jpeg(&engines.vips, &image, 800, 600, 82);

    // `write_config` sets lossless_pass = true; write the "off" variant first.
    let config_path = write_config(dir.path(), &root, "", &engines);
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        text.replace("lossless_pass = true", "lossless_pass = false"),
    )
    .unwrap();

    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);
    let first = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(
        first.totals.files_changed, 0,
        "with the lossless pass off there is nothing to do"
    );
    let settled = Manifest::open(&dir.path().join("state.db")).unwrap();
    assert_eq!(
        settled.count_files().unwrap(),
        1,
        "the decision is remembered"
    );

    // Flip the setting. The stored row's digest no longer matches, so the file must
    // be reconsidered instead of being skipped by the fast path.
    std::fs::write(&config_path, &text).unwrap();
    let cfg2 = load(&config_path, dir.path());
    let second = run::run(&cfg2, &tools, RunOptions { dry_run: false }, "test").unwrap();
    let settled_after = Manifest::open(&dir.path().join("state.db")).unwrap();
    let row = settled_after.get(&image).unwrap().unwrap();
    assert_ne!(
        row.cfg_hash, "",
        "the row must carry the digest that produced it"
    );
    assert!(
        second.totals.files_changed + second.totals.files_rejected > 0,
        "a config change must trigger re-evaluation, not a silent skip"
    );
}

#[test]
fn a_rejected_file_leaves_no_temporary_behind() {
    let Some(engines) = engines() else { return };
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media/2024/05");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("photo.jpg");
    make_jpeg(&engines.vips, &image, 800, 600, 98);

    // A margin no encode can meet guarantees a rejection rather than a change.
    let config_path = write_config(dir.path(), dir.path().join("media").as_path(), "", &engines);
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        text.replace("min_gain_percent = 2.0", "min_gain_percent = 99.0"),
    )
    .unwrap();
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert!(
        report.totals.files_rejected > 0,
        "the run was supposed to reject the file, changed {}",
        report.totals.files_changed
    );

    // The encoded output of a rejected file must not stay behind. Before this was
    // fixed, tens of thousands of these filled the trees.
    let mut leftovers = Vec::new();
    for entry in walkdir::WalkDir::new(dir.path().join("media"))
        .into_iter()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(".mediaopt.") && name.ends_with(".tmp") {
            leftovers.push(entry.path().to_path_buf());
        }
    }
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn an_oversized_image_is_downscaled() {
    let Some(engines) = engines() else { return };
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("huge.jpg");
    // 3000 px wide with max_dimension 2048 forces the resize path, which builds
    // its output filename differently from the plain re-encode. That path wrote to
    // a temporary name ending in .tmp, libvips refused it as an unknown format, and
    // every oversized upload failed while the tests all passed because none of
    // them resized anything.
    make_jpeg(&engines.vips, &image, 3000, 2000, 98);

    let config_path = write_config(dir.path(), &root, "", &engines);
    let cfg = load(&config_path, dir.path());
    let tools = Tools::discover(&cfg.engine);

    let report = run::run(&cfg, &tools, RunOptions { dry_run: false }, "test").unwrap();
    assert_eq!(report.totals.errors, 0, "resizing must not error");
    assert_eq!(report.totals.files_changed, 1);

    let width = mediaopt::engine::header_field(&tools, &image, "width")
        .unwrap()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap();
    let height = mediaopt::engine::header_field(&tools, &image, "height")
        .unwrap()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap();
    assert!(
        width.max(height) <= 2048,
        "longest side should be 2048 or less, got {width}x{height}"
    );
    assert!(
        width >= 2047,
        "the downscale should reach the limit, got {width}"
    );

    // And no leftover partial files.
    for entry in walkdir::WalkDir::new(&root).into_iter().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.contains("mediaopt") && !name.ends_with(".part.jpg"),
            "leftover: {}",
            entry.path().display()
        );
    }
}
