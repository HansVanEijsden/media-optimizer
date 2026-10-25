//! The run orchestrator: walk the roots, decide per file, encode, verify,
//! replace atomically and record the result.
//!
//! Guarantees that matter:
//! * nothing is replaced before it has been decoded and checked,
//! * the manifest row is written after the swap but a failure anywhere leaves the
//!   original untouched (the temporary file is always removed),
//! * a file is only replaced when the result is smaller by the configured margin.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::config::{Config, QualityMode, RootConfig};
use crate::decide::{self, Action, Decision, Facts, Reason};
use crate::engine::{self, JpegEncodeOpts, Tools};
use crate::jpeg;
use crate::limits::{self, Budget, RunLock, Stop};
use crate::manifest::{FileRecord, Fingerprint, Manifest, Outcome, RunTotals};
use crate::probe::{self, Content};
use crate::replace;
use crate::VERSION;

/// Extensions we bother to stat and probe. Everything else in an uploads tree
/// (mp3, mp4, pdf, zip, docx) is not our business.
const IMAGE_EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "jpe", "jfif", "png", "webp", "gif"];

pub struct RunOptions {
    /// Do everything except touch files or the manifest.
    pub dry_run: bool,
}

pub struct RunReport {
    pub run_id: i64,
    /// True when the run only measured and did not write anything.
    pub dry_run: bool,
    pub totals: RunTotals,
    pub stopped: Option<&'static str>,
    pub per_root: Vec<RootReport>,
    pub top_savings: Vec<FileRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct RootReport {
    pub root: PathBuf,
    pub files_seen: u64,
    pub changed: u64,
    pub skipped: u64,
    pub rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub planned_change: u64,
    pub planned_bytes_before: u64,
    pub planned_bytes_after: u64,
}

impl RootReport {
    pub fn bytes_saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

#[derive(Default)]
struct Counters {
    seen: AtomicU64,
    changed: AtomicU64,
    skipped: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
    bytes_before: AtomicU64,
    bytes_after: AtomicU64,
    planned_change: AtomicU64,
    planned_bytes_before: AtomicU64,
    planned_bytes_after: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> [u64; 10] {
        [
            self.seen.load(Ordering::Relaxed),
            self.changed.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.bytes_before.load(Ordering::Relaxed),
            self.bytes_after.load(Ordering::Relaxed),
            self.planned_change.load(Ordering::Relaxed),
            self.planned_bytes_before.load(Ordering::Relaxed),
            self.planned_bytes_after.load(Ordering::Relaxed),
        ]
    }
}

pub struct Runner<'a> {
    cfg: &'a Config,
    tools: &'a Tools,
    manifest: Mutex<Manifest>,
    run_id: i64,
    dry_run: bool,
    budget: Mutex<Budget>,
    stop: AtomicBool,
    stop_reason: Mutex<Option<&'static str>>,
    counters: Counters,
    started: std::time::Instant,
    last_progress: Mutex<std::time::Instant>,
}

impl<'a> Runner<'a> {
    pub fn new(cfg: &'a Config, tools: &'a Tools, dry_run: bool, run_id: i64) -> Result<Self> {
        let manifest = Manifest::open(&cfg.state.db)?;
        let max_runtime = decide::parse_duration(&cfg.limits.max_runtime);
        Ok(Runner {
            cfg,
            tools,
            manifest: Mutex::new(manifest),
            run_id,
            dry_run,
            budget: Mutex::new(Budget::new(
                cfg.limits.max_bytes_per_run,
                cfg.limits.max_files_per_run,
                max_runtime,
            )),
            stop: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
            counters: Counters::default(),
            started: std::time::Instant::now(),
            last_progress: Mutex::new(std::time::Instant::now()),
        })
    }

    pub fn execute(&self) -> Result<RunReport> {
        let mut per_root = Vec::new();
        for root in self.cfg.active_roots().cloned().collect::<Vec<_>>() {
            if !root.path.is_dir() {
                tracing::warn!("root {} does not exist; skipping", root.path.display());
                continue;
            }
            let free = limits::free_bytes(&root.path).unwrap_or(u64::MAX);
            let reserve = decide::parse_size(&self.cfg.limits.free_space_reserve).unwrap_or(0);
            if free < reserve {
                tracing::warn!(
                    "only {} free on {} (reserve {}); skipping this root",
                    limits::human_bytes(free),
                    root.path.display(),
                    limits::human_bytes(reserve)
                );
                continue;
            }
            if !self.dry_run {
                let removed = replace::clean_stale_temps(&root.path);
                if removed > 0 {
                    tracing::info!(
                        "removed {removed} stale temporary file(s) under {}",
                        root.path.display()
                    );
                }
            }
            // A root may carry a tighter budget than the run, which is how a
            // staged first pass is kept to one year at a time.
            *self.budget.lock().unwrap() = Budget::new(
                root.max_bytes_per_run
                    .unwrap_or(self.cfg.limits.max_bytes_per_run),
                root.max_files_per_run
                    .unwrap_or(self.cfg.limits.max_files_per_run),
                decide::parse_duration(&self.cfg.limits.max_runtime),
            );
            let digest = self.cfg.decision_digest(&root);
            let before = self.counters.snapshot();
            self.run_root(&root, &digest);
            let after = self.counters.snapshot();
            per_root.push(RootReport {
                root: root.path.clone(),
                files_seen: after[0] - before[0],
                changed: after[1] - before[1],
                skipped: after[2] - before[2],
                rejected: after[3] - before[3],
                errors: after[4] - before[4],
                bytes_before: after[5] - before[5],
                bytes_after: after[6] - before[6],
                planned_change: after[7] - before[7],
                planned_bytes_before: after[8] - before[8],
                planned_bytes_after: after[9] - before[9],
            });
        }

        let totals = RunTotals {
            files_seen: self.counters.seen.load(Ordering::Relaxed),
            files_changed: self.counters.changed.load(Ordering::Relaxed),
            files_skipped: self.counters.skipped.load(Ordering::Relaxed),
            files_rejected: self.counters.rejected.load(Ordering::Relaxed),
            errors: self.counters.errors.load(Ordering::Relaxed),
            bytes_before: self.counters.bytes_before.load(Ordering::Relaxed),
            bytes_after: self.counters.bytes_after.load(Ordering::Relaxed),
        };
        Ok(RunReport {
            run_id: self.run_id,
            dry_run: self.dry_run,
            totals,
            stopped: *self.stop_reason.lock().unwrap(),
            per_root,
            top_savings: Vec::new(),
        })
    }

    fn run_root(&self, root: &RootConfig, digest: &str) {
        let walker = WalkDir::new(&root.path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_ignored_dir(&e.file_name().to_string_lossy()));
        walker.par_bridge().for_each(|entry| {
            let Ok(entry) = entry else { return };
            if !entry.file_type().is_file() || self.stop.load(Ordering::Relaxed) {
                return;
            }
            if let Err(e) = self.consider(root, entry.path(), digest) {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("{}: {e:#}", entry.path().display());
            }
        });
    }

    fn consider(&self, root: &RootConfig, path: &Path, digest: &str) -> Result<()> {
        if !has_image_extension(path) {
            return Ok(());
        }
        let md = match std::fs::symlink_metadata(path) {
            Ok(md) => md,
            Err(_) => return Ok(()),
        };
        if !md.file_type().is_file() {
            return Ok(());
        }
        if root
            .exclude
            .iter()
            .any(|pat| !pat.is_empty() && path.to_string_lossy().contains(pat.as_str()))
        {
            self.counters.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.counters.seen.fetch_add(1, Ordering::Relaxed);

        let fingerprint = Fingerprint {
            size: md.len(),
            mtime_ns: replace::mtime_ns(&md),
            inode: md.ino(),
        };

        // Fast path: unchanged since we last decided something about it.
        {
            let manifest = self.manifest.lock().unwrap();
            if let Ok(Some((known, outcome, row_digest))) = manifest.fingerprint(path) {
                // `Pending` means a run died mid-file and `Error` means it failed:
                // both are retried. A row decided under different settings is also
                // re-evaluated, so changing the config takes effect immediately.
                if known == fingerprint
                    && row_digest == digest
                    && !matches!(outcome, Outcome::Pending | Outcome::Error)
                {
                    self.counters.skipped.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }

        // Cheap gates before touching the contents.
        if md.len() == 0 || md.len() < self.cfg.limits.min_file_size {
            self.counters.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let min_age = decide::parse_duration(&self.cfg.limits.min_file_age).unwrap_or_default();
        let age = md
            .modified()
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if !min_age.is_zero() && age < min_age {
            self.counters.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if self.budget_reached() {
            return Ok(());
        }

        let content = probe::probe(path).with_context(|| format!("probing {}", path.display()))?;

        // Recovery path: a file carrying our marker is our own output, even when
        // the manifest no longer knows it (restore, rebuild, different server).
        if let Some(info) = &content.jpeg {
            if info.has_tool_marker() {
                self.record_skip(
                    path,
                    root,
                    &content,
                    fingerprint,
                    Reason::AlreadyProcessed,
                    digest,
                )?;
                return Ok(());
            }
        }

        let facts = Facts {
            content: &content,
            size: md.len(),
            age,
        };
        let decision = decide::decide(&facts, self.cfg, root);
        tracing::trace!(
            "{}: format={:?} q={} action={:?} reason={:?} size={}",
            path.display(),
            content.format(),
            content.jpeg.as_ref().map(|j| j.quality).unwrap_or(0),
            decision.action,
            decision.reason,
            md.len()
        );
        if decision.action == Action::Skip {
            self.record_skip(
                path,
                root,
                &content,
                fingerprint,
                decision.reason.unwrap_or(Reason::FormatNotHandled),
                digest,
            )?;
            return Ok(());
        }

        // A dry run goes through the real encoder and then throws the result away:
        // that is the only way to report the exact saving per file before touching
        // anything, which is what a pilot review needs.
        self.maybe_log_progress(path);
        self.optimise(root, path, &content, &decision, fingerprint, digest)
    }

    fn optimise(
        &self,
        root: &RootConfig,
        path: &Path,
        content: &Content,
        decision: &Decision,
        fingerprint: Fingerprint,
        digest: &str,
    ) -> Result<()> {
        // A real run needs its temporary file beside the original so the rename is
        // atomic. A dry run replaces nothing, so it uses the system temporary
        // directory instead and can therefore run against a read-only mount.
        let temp = if self.dry_run {
            replace::scratch_path(path)
        } else {
            replace::temp_path(path)
        };
        match self.try_optimise(root, path, content, decision, &temp, fingerprint, digest) {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                tracing::warn!("{}: {e:#}", path.display());
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                let mut record = self.base_record(path, root, content, fingerprint, digest);
                record.action = decision.action_label().to_string();
                record.outcome = Outcome::Error;
                record.detail = Some(format!("{e:#}"));
                self.store(record)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_optimise(
        &self,
        root: &RootConfig,
        path: &Path,
        content: &Content,
        decision: &Decision,
        temp: &Path,
        fingerprint: Fingerprint,
        digest: &str,
    ) -> Result<()> {
        let size_before = fingerprint.size;
        let meta = replace::read_meta(path)?;
        // Reserve the attempt while holding the budget lock, so four workers
        // cannot all slip past a per-run file budget at the same instant.
        if !self.reserve_attempt() {
            return Ok(());
        }
        let mut quality_used: Option<u8> = None;
        let mut score: Option<f64> = None;
        let lossy = matches!(decision.action, Action::JpegReencode);

        match decision.action {
            Action::JpegReencode => {
                let info = content
                    .jpeg
                    .as_ref()
                    .context("JPEG content vanished between probe and encode")?;
                let opts = JpegEncodeOpts {
                    quality: self.cfg.jpeg.fixed_quality,
                    resize_to: decision.resize_to,
                    source_longest: info.width.max(info.height),
                    source_quality: decision.stored_quality,
                    subsample: self.cfg.jpeg.subsample,
                    source_subsampling: info.subsampling,
                    kernel: self.cfg.jpeg.resize_kernel.clone(),
                    keep: if root.keep.is_empty() {
                        self.cfg.jpeg.keep.clone()
                    } else {
                        root.keep.clone()
                    },
                };
                if decision.resize_to.is_some() {
                    // A resized output cannot be compared to the original pixel for
                    // pixel, so it uses the configured quality without a search.
                    engine::encode_jpeg(self.tools, path, temp, &opts)?;
                    quality_used = Some(opts.quality);
                } else {
                    let (q, s) = self.encode_to_floor(path, temp, &opts)?;
                    quality_used = Some(q);
                    score = s;
                }
            }
            Action::JpegLossless => engine::lossless_jpeg(self.tools, path, temp)?,
            Action::PngQuant => {
                let ok =
                    engine::quantise_png(self.tools, path, temp, self.cfg.png.quality_floor, 1)?;
                if ok {
                    // pngquant's deflate settings are conservative; oxipng on top
                    // finds another 5-15% on palette output, and doing it here
                    // keeps the first pass final (idempotency).
                    engine::lossless_png(temp, temp, self.cfg.png.lossless_effort)?;
                } else {
                    // pngquant declined to meet the quality floor.
                    engine::lossless_png(path, temp, self.cfg.png.lossless_effort)?;
                }
            }
            Action::PngLossless => engine::lossless_png(path, temp, self.cfg.png.lossless_effort)?,
            Action::WebpLossless => {
                anyhow::bail!("lossless WebP re-pack is not implemented in v1")
            }
            Action::Skip => return Ok(()),
        }

        // Verification gate: the output must decode, at the expected size.
        let expect = match decision.resize_to {
            Some(target) => Some(engine::scaled_dimensions(
                content.width(),
                content.height(),
                target,
            )),
            None => Some((content.width(), content.height())),
        };
        engine::verify_decodes(self.tools, temp, expect)
            .with_context(|| format!("verifying encoded {}", path.display()))?;

        // Marker: makes our own output recognisable without the manifest.
        let mut bytes =
            std::fs::read(temp).with_context(|| format!("reading {}", temp.display()))?;
        if matches!(decision.action, Action::JpegReencode | Action::JpegLossless) {
            let descriptor = match decision.action {
                Action::JpegLossless => "ll".to_string(),
                _ => format!("q{}", quality_used.unwrap_or(decision.stored_quality)),
            };
            let marker = jpeg::tool_marker(VERSION, &descriptor);
            if let Some(marked) = jpeg::with_tool_marker(&bytes, &marker) {
                bytes = marked;
                std::fs::write(temp, &bytes)
                    .with_context(|| format!("rewriting {}", temp.display()))?;
            }
        }
        let size_after = bytes.len() as u64;

        // Gain gate. Lossy results need the configured margin (an anomaly guard);
        // lossless results only need to be smaller at all, because they cost no
        // quality whatsoever.
        // Two margins: a lossy result has to earn its quality loss, a lossless
        // result only has to be smaller at all.
        let min_gain = match decision.action {
            Action::JpegReencode => self.cfg.jpeg.min_gain_percent,
            Action::PngQuant => self.cfg.png.min_gain_percent,
            Action::JpegLossless => self.cfg.jpeg.min_gain_percent_lossless,
            Action::PngLossless => self.cfg.png.min_gain_percent_lossless,
            _ => self.cfg.png.min_gain_percent,
        };
        if size_after >= size_before {
            return self.reject(
                root,
                path,
                content,
                decision,
                fingerprint,
                digest,
                size_before,
                size_after,
                "result is not smaller",
            );
        }
        let gain_percent = 100.0 * (size_before - size_after) as f64 / size_before as f64;
        if gain_percent < min_gain {
            return self.reject(
                root,
                path,
                content,
                decision,
                fingerprint,
                digest,
                size_before,
                size_after,
                &format!("gain {gain_percent:.1}% is below the {min_gain:.1}% margin"),
            );
        }
        if lossy {
            if let Some(measured) = score {
                let floor = self.cfg.jpeg.ssimu2_floor;
                if measured < floor {
                    return self.reject(
                        root,
                        path,
                        content,
                        decision,
                        fingerprint,
                        digest,
                        size_before,
                        size_after,
                        &format!("ssimu2 {measured:.1} is below the floor {floor:.1}"),
                    );
                }
            }
        }

        if self.dry_run {
            let _ = std::fs::remove_file(temp);
            self.counters.planned_change.fetch_add(1, Ordering::Relaxed);
            self.counters
                .planned_bytes_before
                .fetch_add(size_before, Ordering::Relaxed);
            self.counters
                .planned_bytes_after
                .fetch_add(size_after, Ordering::Relaxed);
            tracing::trace!(
                "{}: would shrink {} -> {} ({gain_percent:.1}%)",
                path.display(),
                size_before,
                size_after
            );
            return Ok(());
        }

        // The swap.
        replace::finish_replace(temp, path, &meta)?;

        self.counters.changed.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_before
            .fetch_add(size_before, Ordering::Relaxed);
        self.counters
            .bytes_after
            .fetch_add(size_after, Ordering::Relaxed);
        self.budget.lock().unwrap().spent(size_before);

        // `sha_after` is the content identity of the file we just wrote. The input
        // hash is deliberately not computed: it would mean a second full read of
        // every file for a value nothing needs, since path plus the stored
        // fingerprint already identify the row.
        let sha_after = replace::sha256_file(path).ok();
        let mut record = self.base_record(path, root, content, fingerprint, digest);
        record.sha_before = None;
        // The fast path must look at the file as it is *now*, not as it was.
        if let Ok(md) = std::fs::symlink_metadata(path) {
            record.fp_size = md.len();
            record.fp_mtime_ns = replace::mtime_ns(&md);
            record.fp_inode = md.ino();
        }
        record.size_after = Some(size_after);
        record.sha_after = sha_after;
        record.action = decision.action_label().to_string();
        record.quality = quality_used.unwrap_or(decision.stored_quality);
        record.ssimu2 = score;
        record.encoder = Some("libvips".to_string());
        record.encoder_version = self.tools.vips.as_ref().map(|t| t.version.clone());
        record.settings = Some(self.settings_summary(decision, quality_used));
        record.outcome = Outcome::Changed;
        record.detail = Some(format!(
            "{size_before} -> {size_after} ({gain_percent:.1}%)"
        ));
        self.store(record)
    }

    /// Encode at a descending quality ladder until the SSIMULACRA2 floor is
    /// missed, then keep the lowest quality that still met it.
    ///
    /// A fixed quality is either wasteful (files that reach the floor much lower)
    /// or risky (files that need more), and the required quality for the same
    /// floor varies from Q70 to Q92 across a real corpus.
    fn encode_to_floor(
        &self,
        src: &Path,
        temp: &Path,
        base: &JpegEncodeOpts,
    ) -> Result<(u8, Option<f64>)> {
        if self.cfg.jpeg.quality_mode == QualityMode::Fixed || self.tools.ssimulacra2.is_none() {
            engine::encode_jpeg(self.tools, src, temp, base)?;
            let score = engine::ssimu2(self.tools, src, temp).unwrap_or(None);
            return Ok((base.quality, score));
        }
        let floor = self.cfg.jpeg.ssimu2_floor;
        // Start the search near the stored quality: a q99 original needs a much
        // higher output quality than a q88 one to hold the same perceptual floor,
        // so guessing from `fixed_quality` alone costs two extra encodes per file.
        let start = if base.source_quality >= 90 {
            (base.source_quality as i32 - 11).clamp(70, 92)
        } else {
            base.quality as i32
        };
        let mut last_encoded: i32 = start;
        let mut last_score = self.encode_and_measure(src, temp, base, last_encoded)?;
        let mut best: Option<(i32, f64)> = None;

        if last_score >= floor {
            best = Some((last_encoded, last_score));
            loop {
                let next = last_encoded - 4;
                if next < 60 {
                    break;
                }
                let s = self.encode_and_measure(src, temp, base, next)?;
                last_encoded = next;
                last_score = s;
                if s >= floor {
                    best = Some((next, s));
                } else {
                    break;
                }
            }
        } else {
            let mut up = last_encoded + 3;
            while up <= 96 {
                let s = self.encode_and_measure(src, temp, base, up)?;
                last_encoded = up;
                last_score = s;
                if s >= floor {
                    best = Some((up, s));
                    break;
                }
                up += 3;
            }
        }

        let (q_final, s_final) = best.unwrap_or((last_encoded, last_score));
        if last_encoded != q_final {
            // The last write was a probe that missed the floor: redo the winner.
            let mut opts = base.clone();
            opts.quality = q_final as u8;
            engine::encode_jpeg(self.tools, src, temp, &opts)?;
        }
        Ok((q_final as u8, Some(s_final)))
    }

    fn encode_and_measure(
        &self,
        src: &Path,
        temp: &Path,
        base: &JpegEncodeOpts,
        quality: i32,
    ) -> Result<f64> {
        let mut opts = base.clone();
        opts.quality = quality.clamp(40, 96) as u8;
        engine::encode_jpeg(self.tools, src, temp, &opts)?;
        let score = engine::ssimu2(self.tools, src, temp)?
            .context("ssimulacra2 produced no score while quality_mode = target")?;
        Ok(score)
    }

    fn settings_summary(&self, decision: &Decision, quality: Option<u8>) -> String {
        match decision.action {
            Action::JpegReencode => match decision.resize_to {
                Some(target) => format!(
                    "resize={target} kernel={} q={}",
                    self.cfg.jpeg.resize_kernel,
                    quality.unwrap_or(self.cfg.jpeg.fixed_quality)
                ),
                None => match self.cfg.jpeg.quality_mode {
                    QualityMode::Target => format!(
                        "ssimu2_target={} q={}",
                        self.cfg.jpeg.ssimu2_floor,
                        quality.unwrap_or(0)
                    ),
                    QualityMode::Fixed => format!("fixed q={}", quality.unwrap_or(0)),
                },
            },
            Action::JpegLossless => "jpegtran -optimize -progressive -copy all".to_string(),
            Action::PngQuant => format!("pngquant quality>={}", self.cfg.png.quality_floor),
            Action::PngLossless => format!("oxipng -o {}", self.cfg.png.lossless_effort),
            _ => String::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reject(
        &self,
        root: &RootConfig,
        path: &Path,
        content: &Content,
        decision: &Decision,
        fingerprint: Fingerprint,
        digest: &str,
        size_before: u64,
        size_after: u64,
        why: &str,
    ) -> Result<()> {
        self.counters.rejected.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("{}: keeping original ({why})", path.display());
        let mut record = self.base_record(path, root, content, fingerprint, digest);
        record.action = decision.action_label().to_string();
        record.size_after = Some(size_after);
        record.outcome = Outcome::Rejected;
        record.detail = Some(format!(
            "kept original: {why} ({size_before} -> {size_after})"
        ));
        self.store(record)
    }

    fn record_skip(
        &self,
        path: &Path,
        root: &RootConfig,
        content: &Content,
        fingerprint: Fingerprint,
        reason: Reason,
        digest: &str,
    ) -> Result<()> {
        self.counters.skipped.fetch_add(1, Ordering::Relaxed);
        let mut record = self.base_record(path, root, content, fingerprint, digest);
        record.action = "skip".to_string();
        record.reason = Some(reason.as_str().to_string());
        record.outcome = Outcome::Skipped;
        record.detail = Some(reason.label().to_string());
        self.store(record)
    }

    fn base_record(
        &self,
        path: &Path,
        root: &RootConfig,
        content: &Content,
        fingerprint: Fingerprint,
        digest: &str,
    ) -> FileRecord {
        FileRecord {
            path: path.to_path_buf(),
            root: root.path.clone(),
            size_before: fingerprint.size,
            mtime_ns: fingerprint.mtime_ns,
            inode: fingerprint.inode,
            fp_size: fingerprint.size,
            fp_mtime_ns: fingerprint.mtime_ns,
            fp_inode: fingerprint.inode,
            content_format: content.format().label().to_string(),
            width: content.width(),
            height: content.height(),
            quality: content.jpeg.as_ref().map(|j| j.quality).unwrap_or(0),
            quality_confident: content
                .jpeg
                .as_ref()
                .map(|j| j.quality_confident)
                .unwrap_or(false),
            run_id: Some(self.run_id),
            cfg_hash: digest.to_string(),
            ..Default::default()
        }
    }

    fn store(&self, record: FileRecord) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let manifest = self.manifest.lock().unwrap();
        manifest.upsert(&record)
    }

    /// Claim one unit of the file budget. Returns false when the run is spent.
    fn reserve_attempt(&self) -> bool {
        let mut budget = self.budget.lock().unwrap();
        if let Stop::Exhausted(reason) = budget.check() {
            drop(budget);
            self.mark_stopped(reason);
            return false;
        }
        budget.attempted();
        true
    }

    fn mark_stopped(&self, reason: &'static str) {
        self.stop.store(true, Ordering::Relaxed);
        let mut slot = self.stop_reason.lock().unwrap();
        if slot.is_none() {
            tracing::info!("stopping early: {reason} reached");
            *slot = Some(reason);
        }
    }

    /// One INFO line a minute, so a run that lasts all night is not silent and an
    /// operator can see which directory it is in without turning on debug logs.
    fn maybe_log_progress(&self, path: &Path) {
        // Operators watching a pilot want a faster heartbeat than one a minute;
        // MEDIAOPT_PROGRESS_SECS overrides it without a rebuild.
        let every = std::env::var("MEDIAOPT_PROGRESS_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(60);
        let mut last = self.last_progress.lock().unwrap();
        if last.elapsed() < std::time::Duration::from_secs(every) {
            return;
        }
        *last = std::time::Instant::now();
        let seen = self.counters.seen.load(Ordering::Relaxed);
        let changed = self.counters.changed.load(Ordering::Relaxed);
        let rejected = self.counters.rejected.load(Ordering::Relaxed);
        let errors = self.counters.errors.load(Ordering::Relaxed);
        let before = self.counters.bytes_before.load(Ordering::Relaxed);
        let after = self.counters.bytes_after.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed().as_secs_f64().max(1.0);
        tracing::info!(
            "progress: dir={} seen={} changed={} rejected={} errors={} freed={} rate={:.1}/s",
            path.parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            seen,
            changed,
            rejected,
            errors,
            limits::human_bytes(before.saturating_sub(after)),
            seen as f64 / elapsed
        );
    }

    fn budget_reached(&self) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return true;
        }
        // A container stop must not wait for a whole tree. The scheduler installs
        // the handler, but a plain `run` gets it too, so Ctrl-C and `docker stop`
        // both finish the file in flight and exit cleanly.
        if crate::sched::shutdown_requested() {
            self.mark_stopped("signal");
            return true;
        }
        match self.budget.lock().unwrap().check() {
            Stop::Continue => false,
            Stop::Exhausted(reason) => {
                self.mark_stopped(reason);
                true
            }
        }
    }
}

impl Decision {
    /// Machine-readable action name stored in the manifest.
    pub fn action_label(&self) -> &'static str {
        match self.action {
            Action::JpegReencode => "jpeg_reencode",
            Action::JpegLossless => "jpeg_lossless",
            Action::PngQuant => "png_quant",
            Action::PngLossless => "png_lossless",
            Action::WebpLossless => "webp_lossless",
            Action::Skip => "skip",
        }
    }
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::TooSmall => "too_small",
            Reason::TooNew => "too_new",
            Reason::ZeroBytes => "zero_bytes",
            Reason::FormatNotHandled => "format_not_handled",
            Reason::UnparsableHeader => "unparsable_header",
            Reason::Cmyk => "cmyk",
            Reason::QualityAlreadyLean => "quality_already_lean",
            Reason::DimensionOk => "dimension_ok",
            Reason::PngHasAlpha => "png_has_alpha",
            Reason::WebpLossy => "webp_lossy",
            Reason::FormatDisabled => "format_disabled",
            Reason::NoGainKnown => "no_gain_known",
            Reason::LosslessDisabled => "lossless_disabled",
            Reason::AlreadyProcessed => "already_processed",
            Reason::PngHasIcc => "png_has_icc",
        }
    }
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .map(|e| {
            let e = e.to_string_lossy().to_ascii_lowercase();
            IMAGE_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// Directories that never contain media we should touch.
fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".svn" | "node_modules" | ".cache" | ".trash" | "@eaDir" | "#recycle"
    )
}

/// Entry point used by `main` and by the scheduler.
pub fn run(cfg: &Config, tools: &Tools, options: RunOptions, mode: &str) -> Result<RunReport> {
    let host = crate::hostname();
    crate::sched::install_signal_handlers();
    let lock_path = cfg.state.db.with_extension("lock");
    let _lock = RunLock::acquire(&lock_path)?;
    let priority = limits::apply_low_priority();
    if let Some(nice) = priority.nice {
        tracing::info!("running at nice {nice}, idle I/O: {}", priority.idle_io);
    }

    let manifest = Manifest::open(&cfg.state.db)?;
    let run_id = manifest.start_run(&host, VERSION, mode)?;
    manifest.set_meta("last_host", &host)?;
    drop(manifest);

    let runner = Runner::new(cfg, tools, options.dry_run, run_id)?;
    let mut report = runner.execute()?;

    let manifest = Manifest::open(&cfg.state.db)?;
    let note = report.stopped.map(|s| format!("stopped early: {s}"));
    manifest.finish_run(run_id, &report.totals, note.as_deref())?;
    report.top_savings = manifest.changed_in_run(run_id).unwrap_or_default();
    Ok(report)
}

/// Used by the tests and by the report to reason about extensions.
pub fn extension_is_image(path: &Path) -> bool {
    has_image_extension(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_filter() {
        assert!(extension_is_image(Path::new("/a/b/photo.JPG")));
        assert!(extension_is_image(Path::new("/a/b/photo.jpeg")));
        assert!(extension_is_image(Path::new("/a/b/photo.png")));
        assert!(!extension_is_image(Path::new("/a/b/song.mp3")));
        assert!(!extension_is_image(Path::new("/a/b/archive.tar.gz")));
        assert!(!extension_is_image(Path::new("/a/b/noext")));
    }

    #[test]
    fn ignored_directories() {
        assert!(is_ignored_dir("node_modules"));
        assert!(is_ignored_dir("@eaDir"));
        assert!(!is_ignored_dir("2024"));
    }
}
