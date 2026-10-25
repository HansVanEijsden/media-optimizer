//! Remote mode: let a fast machine do the encoding while the server only stores.
//!
//! The split exists because the expensive part of this tool is the encoder, not
//! the disk. A server with two shared vCPUs needs days for a million-file
//! backlog; a laptop can do the same work in hours. The protocol is deliberately
//! dumb and stateless on the client side:
//!
//! 1. `scan` runs on the server, next to the manifest, and prints one TSV line
//!    per file that still needs work, including the action that was decided.
//! 2. `encode-batch` runs on the fast machine over a downloaded batch. It has no
//!    manifest and no knowledge of the server; it only produces files.
//! 3. `apply-staged` runs on the server again and re-verifies every result before
//!    it replaces anything: decodes, dimensions, size and the configured margin.
//!    A buggy or stale batch can therefore waste work but cannot damage the tree.
//!
//! The server keeps the manifest, so a normal nightly run picks up exactly where
//! the remote pass stopped, and vice versa.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::config::{Config, QualityMode, RootConfig};
use crate::decide::{self, Action, Facts};
use crate::engine::{self, JpegEncodeOpts, Tools};
use crate::jpeg;
use crate::manifest::{Fingerprint, Manifest, Outcome};
use crate::probe;
use crate::replace;
use crate::run::extension_is_image;
use crate::VERSION;

/// One file that needs work, as agreed between the server and the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub index: u64,
    pub path: PathBuf,
    pub action: String,
    pub size: u64,
}

impl Candidate {
    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}",
            self.index,
            self.action,
            self.size,
            self.path.display()
        )
    }

    pub fn from_tsv(line: &str) -> Option<Candidate> {
        let mut parts = line.splitn(4, '\t');
        let index = parts.next()?.parse().ok()?;
        let action = parts.next()?.to_string();
        let size = parts.next()?.parse().ok()?;
        let path = PathBuf::from(parts.next()?);
        Some(Candidate {
            index,
            path,
            action,
            size,
        })
    }
}

/// The outcome of encoding one file on the client.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedResult {
    pub index: u64,
    pub path: PathBuf,
    pub action: String,
    pub status: &'static str,
    pub quality: u8,
    pub ssimu2: Option<f64>,
    pub size_before: u64,
    pub size_after: u64,
    pub note: String,
}

impl StagedResult {
    /// Self-contained: the path travels with the result, so the server needs
    /// nothing else to apply a batch.
    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.index,
            self.action,
            self.status,
            self.quality,
            self.ssimu2
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "-".to_string()),
            self.size_before,
            self.size_after,
            self.path.display(),
            self.note.replace('\t', " ")
        )
    }

    pub fn from_tsv(line: &str) -> Option<StagedResult> {
        let fields: Vec<&str> = line.splitn(9, '\t').collect();
        if fields.len() < 9 {
            return None;
        }
        Some(StagedResult {
            index: fields[0].parse().ok()?,
            action: fields[1].to_string(),
            status: Box::leak(fields[2].to_string().into_boxed_str()),
            quality: fields[3].parse().unwrap_or(0),
            ssimu2: fields[4].parse().ok(),
            size_before: fields[5].parse().ok()?,
            size_after: fields[6].parse().ok()?,
            path: PathBuf::from(fields[7]),
            note: fields[8].to_string(),
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct RemoteStats {
    pub candidates: u64,
    pub encoded: u64,
    pub changed: u64,
    pub rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl RemoteStats {
    pub fn saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Server side: walk the roots, apply the manifest fast path and the decision
/// engine, and write one TSV line per file that still needs work.
///
/// Nothing is modified. The digest of the active settings is written to stderr as
/// a comment line so the client can tell where the list came from.
pub fn scan(
    cfg: &Config,
    root_filter: Option<&Path>,
    limit: u64,
    out: &mut dyn Write,
) -> Result<RemoteStats> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let min_age = decide::parse_duration(&cfg.limits.min_file_age).unwrap_or_default();
    let mut stats = RemoteStats::default();
    let mut index = 0u64;

    let roots: Vec<RootConfig> = cfg
        .active_roots()
        .filter(|r| root_filter.map(|f| r.path == f).unwrap_or(true))
        .cloned()
        .collect();
    if roots.is_empty() {
        bail!("no configured root matches the requested filter");
    }

    for root in roots {
        if !root.path.is_dir() {
            tracing::warn!("root {} does not exist", root.path.display());
            continue;
        }
        let digest = cfg.decision_digest(&root);
        let walker = WalkDir::new(&root.path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_string_lossy().as_ref(),
                    ".git" | "node_modules" | "@eaDir" | "#recycle"
                )
            });
        for entry in walker {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() || !extension_is_image(entry.path()) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let size = md.len();
            if size == 0 || size < cfg.limits.min_file_size {
                continue;
            }
            let age = md
                .modified()
                .ok()
                .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
                .unwrap_or(std::time::Duration::ZERO);
            if !min_age.is_zero() && age < min_age {
                continue;
            }
            use std::os::unix::fs::MetadataExt;
            let fingerprint = Fingerprint {
                size,
                mtime_ns: replace::mtime_ns(&md),
                inode: md.ino(),
            };
            if let Ok(Some((known, outcome, row_digest))) = manifest.fingerprint(entry.path()) {
                if known == fingerprint
                    && row_digest == digest
                    && !matches!(outcome, Outcome::Pending | Outcome::Error)
                {
                    continue;
                }
            }
            let Ok(content) = probe::probe(entry.path()) else {
                continue;
            };
            if content
                .jpeg
                .as_ref()
                .map(|j| j.has_tool_marker())
                .unwrap_or(false)
            {
                continue;
            }
            let facts = Facts {
                content: &content,
                size,
                age,
            };
            let decision = decide::decide(&facts, cfg, &root);
            if decision.action == Action::Skip {
                continue;
            }
            stats.candidates += 1;
            index += 1;
            let line = format!(
                "{}\t{}\t{}\t{}\n",
                index,
                decision.action_label(),
                size,
                entry.path().display()
            );
            out.write_all(line.as_bytes())?;
            if limit > 0 && stats.candidates >= limit {
                return Ok(stats);
            }
        }
    }
    Ok(stats)
}

/// Client side: encode the files of one batch.
///
/// `input` holds the downloaded files named after their index; `output` receives
/// the encoded files, also named after their index. No manifest, no state.
pub fn encode_batch(
    cfg: &Config,
    tools: &Tools,
    candidates: &[Candidate],
    input: &Path,
    output: &Path,
    jobs: usize,
) -> Result<Vec<StagedResult>> {
    std::fs::create_dir_all(output)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()?;
    let results = pool.install(|| {
        candidates
            .par_iter()
            .map(|cand| match encode_one(cfg, tools, cand, input, output) {
                Ok(result) => result,
                Err(e) => StagedResult {
                    index: cand.index,
                    path: cand.path.clone(),
                    action: cand.action.clone(),
                    status: "error",
                    quality: 0,
                    ssimu2: None,
                    size_before: cand.size,
                    size_after: cand.size,
                    note: format!("{e:#}"),
                },
            })
            .collect::<Vec<_>>()
    });
    Ok(results)
}

fn encode_one(
    cfg: &Config,
    tools: &Tools,
    cand: &Candidate,
    input: &Path,
    output: &Path,
) -> Result<StagedResult> {
    // The download preserves the tree, so the local copy sits at the same
    // relative path it has on the server. Output is index-named: it only has to
    // be unique inside one batch.
    let relative = cand.path.strip_prefix("/").unwrap_or(cand.path.as_path());
    let src = input.join(relative);
    let dst = output.join(cand.index.to_string());
    let content = probe::probe(&src).with_context(|| format!("probing {}", src.display()))?;
    let mut quality_used = 0u8;
    let mut score = None;

    match cand.action.as_str() {
        "jpeg_reencode" => {
            let info = content.jpeg.as_ref().context("not a JPEG")?;
            let longest = info.width.max(info.height);
            let opts = JpegEncodeOpts {
                quality: cfg.jpeg.fixed_quality,
                resize_to: (cfg.jpeg.max_dimension > 0 && longest > cfg.jpeg.max_dimension)
                    .then_some(cfg.jpeg.max_dimension),
                source_longest: longest,
                source_quality: info.quality,
                subsample: cfg.jpeg.subsample,
                source_subsampling: info.subsampling,
                kernel: cfg.jpeg.resize_kernel.clone(),
                keep: cfg.jpeg.keep.clone(),
            };
            if opts.resize_to.is_some() || cfg.jpeg.quality_mode == QualityMode::Fixed {
                engine::encode_jpeg(tools, &src, &dst, &opts)?;
                quality_used = opts.quality;
                score = engine::ssimu2(tools, &src, &dst).unwrap_or(None);
            } else {
                let (q, s) = search_quality(cfg, tools, &src, &dst, &opts)?;
                quality_used = q;
                score = s;
            }
        }
        "jpeg_lossless" => engine::lossless_jpeg(tools, &src, &dst)?,
        "png_quant" => {
            let ok = engine::quantise_png(tools, &src, &dst, cfg.png.quality_floor, 1)?;
            if ok {
                engine::lossless_png(&dst, &dst, cfg.png.lossless_effort)?;
            } else {
                engine::lossless_png(&src, &dst, cfg.png.lossless_effort)?;
            }
        }
        "png_lossless" => engine::lossless_png(&src, &dst, cfg.png.lossless_effort)?,
        other => bail!("action {other} is not handled by the remote encoder"),
    }

    // Same verification gate as a local run.
    let expect = if cand.action == "jpeg_reencode" {
        let info = content.jpeg.as_ref().context("not a JPEG")?;
        let target = cfg.jpeg.max_dimension;
        if target > 0 && info.width.max(info.height) > target {
            Some(engine::scaled_dimensions(info.width, info.height, target))
        } else {
            Some((info.width, info.height))
        }
    } else {
        Some((content.width(), content.height()))
    };
    engine::verify_decodes(tools, &dst, expect)?;

    // Marker, so the server can recognise the file even without a manifest.
    if cand.action.starts_with("jpeg") {
        let descriptor = if cand.action == "jpeg_lossless" {
            "ll".to_string()
        } else {
            format!("q{quality_used}")
        };
        let bytes = std::fs::read(&dst)?;
        if let Some(marked) =
            jpeg::with_tool_marker(&bytes, &jpeg::tool_marker(VERSION, &descriptor))
        {
            std::fs::write(&dst, marked)?;
        }
    }

    let size_after = std::fs::metadata(&dst)?.len();
    let min_gain = match cand.action.as_str() {
        "jpeg_reencode" | "png_quant" => cfg.jpeg.min_gain_percent,
        _ => cfg.jpeg.min_gain_percent_lossless,
    };
    let mut status = "ok";
    let mut note = String::new();
    if size_after >= cand.size {
        status = "no_gain";
        note.push_str("not smaller");
    } else {
        let gain = 100.0 * (cand.size - size_after) as f64 / cand.size as f64;
        if gain < min_gain {
            status = "no_gain";
            note.push_str(&format!("gain {gain:.1}% below {min_gain:.1}%"));
        } else {
            note.push_str(&format!("{gain:.1}%"));
        }
    }
    if status == "ok" && cand.action == "jpeg_reencode" {
        if let Some(measured) = score {
            if measured < cfg.jpeg.ssimu2_floor {
                status = "no_gain";
                note = format!("ssimu2 {measured:.1} below {}", cfg.jpeg.ssimu2_floor);
            }
        }
    }

    Ok(StagedResult {
        index: cand.index,
        path: cand.path.clone(),
        action: cand.action.clone(),
        status: Box::leak(status.to_string().into_boxed_str()),
        quality: quality_used,
        ssimu2: score,
        size_before: cand.size,
        size_after,
        note,
    })
}

/// The same descending quality search the local runner uses.
fn search_quality(
    cfg: &Config,
    tools: &Tools,
    src: &Path,
    dst: &Path,
    base: &JpegEncodeOpts,
) -> Result<(u8, Option<f64>)> {
    let floor = cfg.jpeg.ssimu2_floor;
    if tools.ssimulacra2.is_none() {
        engine::encode_jpeg(tools, src, dst, base)?;
        return Ok((base.quality, None));
    }
    let start = if base.source_quality >= 90 {
        (base.source_quality as i32 - 11).clamp(70, 92)
    } else {
        base.quality as i32
    };
    let measure = |q: i32| -> Result<(i32, f64)> {
        let mut opts = base.clone();
        opts.quality = q.clamp(40, 96) as u8;
        engine::encode_jpeg(tools, src, dst, &opts)?;
        let score = engine::ssimu2(tools, src, dst)?
            .context("ssimulacra2 produced no score while quality_mode = target")?;
        Ok((q, score))
    };
    let (mut last_q, mut last_score) = measure(start)?;
    let mut best: Option<(i32, f64)> = None;
    if last_score >= floor {
        best = Some((last_q, last_score));
        loop {
            let next = last_q - 4;
            if next < 60 {
                break;
            }
            let (q, s) = measure(next)?;
            last_q = q;
            last_score = s;
            if s >= floor {
                best = Some((q, s));
            } else {
                break;
            }
        }
    } else {
        let mut up = last_q + 3;
        while up <= 96 {
            let (q, s) = measure(up)?;
            last_q = q;
            last_score = s;
            if s >= floor {
                best = Some((q, s));
                break;
            }
            up += 3;
        }
    }
    let (q_final, s_final) = best.unwrap_or((last_q, last_score));
    if last_q != q_final {
        let mut opts = base.clone();
        opts.quality = q_final as u8;
        engine::encode_jpeg(tools, src, dst, &opts)?;
    }
    Ok((q_final as u8, Some(s_final)))
}

/// Server side: replace the originals with the verified staged results.
///
/// Every result is re-checked against the live file: the decode must succeed at
/// the right size, the file must be smaller, and the configured margin must hold.
/// Only then is it renamed into place, with owner, mode and mtime preserved, and
/// the manifest row written.
pub fn apply_staged(
    cfg: &Config,
    tools: &Tools,
    stage: &Path,
    results: &[StagedResult],
) -> Result<RemoteStats> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let run_id = manifest.start_run(&crate::hostname(), VERSION, "remote-apply")?;
    let mut stats = RemoteStats::default();
    let mut records = Vec::new();

    for result in results {
        stats.encoded += 1;
        if result.status != "ok" {
            stats.rejected += 1;
            // Remember the rejection, otherwise every later scan would list the
            // file again and the client would encode it again for nothing.
            if let Ok(md) = std::fs::symlink_metadata(&result.path) {
                use std::os::unix::fs::MetadataExt;
                let fingerprint = Fingerprint {
                    size: md.len(),
                    mtime_ns: replace::mtime_ns(&md),
                    inode: md.ino(),
                };
                let mut record = crate::manifest::FileRecord {
                    path: result.path.clone(),
                    root: cfg
                        .find_root(&result.path)
                        .map(|r| r.path.clone())
                        .unwrap_or_default(),
                    size_before: fingerprint.size,
                    mtime_ns: fingerprint.mtime_ns,
                    inode: fingerprint.inode,
                    fp_size: fingerprint.size,
                    fp_mtime_ns: fingerprint.mtime_ns,
                    fp_inode: fingerprint.inode,
                    size_after: Some(result.size_after),
                    content_format: "jpeg".to_string(),
                    action: result.action.clone(),
                    outcome: Outcome::Rejected,
                    detail: Some(format!("remote: {}", result.note)),
                    run_id: Some(run_id),
                    cfg_hash: cfg
                        .find_root(&result.path)
                        .map(|r| cfg.decision_digest(r))
                        .unwrap_or_default(),
                    ..Default::default()
                };
                record.quality = result.quality;
                records.push(record);
            }
            continue;
        }
        let staged = stage.join(result.index.to_string());
        let target = &result.path;
        let Ok(meta) = replace::read_meta(target) else {
            stats.errors += 1;
            tracing::warn!("{}: disappeared before apply", target.display());
            continue;
        };
        let Ok(md) = std::fs::symlink_metadata(target) else {
            stats.errors += 1;
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        let fingerprint = Fingerprint {
            size: md.len(),
            mtime_ns: replace::mtime_ns(&md),
            inode: md.ino(),
        };
        // The original may have changed since the scan; refuse to overwrite it.
        if fingerprint.size != result.size_before {
            stats.errors += 1;
            tracing::warn!(
                "{}: size changed since the scan ({} != {})",
                target.display(),
                fingerprint.size,
                result.size_before
            );
            continue;
        }
        if let Err(e) = engine::verify_decodes(tools, &staged, None) {
            stats.errors += 1;
            tracing::warn!("{}: staged file does not decode: {e:#}", target.display());
            continue;
        }
        let staged_size = std::fs::metadata(&staged)?.len();
        if staged_size >= result.size_before {
            stats.rejected += 1;
            continue;
        }
        let gain = 100.0 * (result.size_before - staged_size) as f64 / result.size_before as f64;
        let min_gain = match result.action.as_str() {
            "jpeg_reencode" | "png_quant" => cfg.jpeg.min_gain_percent,
            _ => cfg.jpeg.min_gain_percent_lossless,
        };
        if gain < min_gain {
            stats.rejected += 1;
            continue;
        }
        if let Err(e) = replace::finish_replace(&staged, target, &meta) {
            stats.errors += 1;
            tracing::warn!("{}: replace failed: {e:#}", target.display());
            continue;
        }
        stats.changed += 1;
        stats.bytes_before += result.size_before;
        stats.bytes_after += staged_size;

        let mut record = crate::manifest::FileRecord {
            path: target.clone(),
            root: cfg
                .find_root(target)
                .map(|r| r.path.clone())
                .unwrap_or_default(),
            size_before: result.size_before,
            mtime_ns: fingerprint.mtime_ns,
            inode: fingerprint.inode,
            size_after: Some(staged_size),
            sha_after: replace::sha256_file(target).ok(),
            content_format: "jpeg".to_string(),
            quality: result.quality,
            quality_confident: true,
            action: result.action.clone(),
            ssimu2: result.ssimu2,
            encoder: Some("remote".to_string()),
            encoder_version: Some(VERSION.to_string()),
            settings: Some(format!("remote batch: {}", result.note)),
            outcome: Outcome::Changed,
            detail: Some(format!("{gain:.1}% via remote")),
            run_id: Some(run_id),
            cfg_hash: cfg
                .find_root(target)
                .map(|r| cfg.decision_digest(r))
                .unwrap_or_default(),
            ..Default::default()
        };
        if let Ok(after) = std::fs::symlink_metadata(target) {
            record.fp_size = after.len();
            record.fp_mtime_ns = replace::mtime_ns(&after);
            record.fp_inode = after.ino();
        }
        records.push(record);
    }

    for record in records {
        manifest.upsert(&record)?;
    }
    let totals = crate::manifest::RunTotals {
        files_seen: stats.encoded,
        files_changed: stats.changed,
        files_skipped: 0,
        files_rejected: stats.rejected,
        errors: stats.errors,
        bytes_before: stats.bytes_before,
        bytes_after: stats.bytes_after,
    };
    manifest.finish_run(run_id, &totals, Some("remote apply"))?;
    Ok(stats)
}

/// Parse a scan TSV (as produced by `scan`) into candidates.
pub fn read_candidates(path: &Path) -> Result<Vec<Candidate>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening candidate list {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(c) = Candidate::from_tsv(&line) {
            out.push(c);
        }
    }
    Ok(out)
}

/// Parse a result TSV (as produced by `encode-batch`).
pub fn read_results(path: &Path) -> Result<Vec<StagedResult>> {
    if path == Path::new("-") {
        let stdin = std::io::stdin();
        let lines: Vec<String> = stdin.lock().lines().collect::<std::io::Result<Vec<_>>>()?;
        return parse_results(lines.iter().map(|s| s.as_str()));
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening result list {}", path.display()))?;
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()?;
    parse_results(lines.iter().map(|s| s.as_str()))
}

fn parse_results<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Vec<StagedResult>> {
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(r) = StagedResult::from_tsv(line) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Convenience wrapper used by the CLI to write a batch list to a file.
pub fn write_candidates(path: &Path, candidates: &[Candidate]) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    for c in candidates {
        writeln!(file, "{}", c.to_tsv())?;
    }
    Ok(())
}

/// Run a shell command and return its stdout, failing on a non-zero exit.
pub fn run_command(program: &str, args: &[String]) -> Result<std::process::Output> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_tsv_round_trips() {
        let c = Candidate {
            index: 7,
            path: PathBuf::from("/home/example/www/a b.jpg"),
            action: "jpeg_reencode".into(),
            size: 1234,
        };
        let parsed = Candidate::from_tsv(&c.to_tsv()).unwrap();
        assert_eq!(parsed, c, "paths with spaces must survive the round trip");
    }

    #[test]
    fn result_tsv_round_trips() {
        let r = StagedResult {
            index: 3,
            path: PathBuf::from("/home/example/www/a.jpg"),
            action: "jpeg_lossless".into(),
            status: "ok",
            quality: 0,
            ssimu2: Some(78.5),
            size_before: 1000,
            size_after: 900,
            note: "10.0%".into(),
        };
        let parsed = StagedResult::from_tsv(&r.to_tsv()).unwrap();
        assert_eq!(parsed.index, r.index);
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.ssimu2, Some(78.5));
        assert_eq!(parsed.size_after, 900);
        assert_eq!(parsed.path, r.path, "the path must travel with the result");
    }

    #[test]
    fn missing_ssimu2_is_a_dash() {
        let r = StagedResult {
            index: 1,
            path: PathBuf::new(),
            action: "jpeg_lossless".into(),
            status: "no_gain",
            quality: 0,
            ssimu2: None,
            size_before: 10,
            size_after: 10,
            note: String::new(),
        };
        assert!(r.to_tsv().contains("\t-\t"));
        assert_eq!(StagedResult::from_tsv(&r.to_tsv()).unwrap().ssimu2, None);
    }

    #[test]
    fn garbage_lines_are_ignored() {
        assert!(Candidate::from_tsv("nonsense").is_none());
        assert!(Candidate::from_tsv("1\t2\t3").is_none());
        assert!(StagedResult::from_tsv("1\t2").is_none());
    }
}
