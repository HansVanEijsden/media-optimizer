//! Reporting: a human summary on stdout, a JSON record per run on disk, and the
//! one-line summary an operator can grep out of syslog.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

use crate::limits::human_bytes;
use crate::manifest::{Manifest, RootSummary, RunSummary};
use crate::run::RunReport;
use crate::VERSION;

#[derive(Debug, Serialize)]
pub struct RunFile {
    pub tool_version: String,
    pub run_id: i64,
    pub started: String,
    pub host: String,
    pub mode: String,
    pub stopped: Option<String>,
    pub totals: Totals,
    pub per_root: Vec<RootJson>,
    pub top_savings: Vec<ChangeJson>,
    pub engines: String,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub files_seen: u64,
    pub files_changed: u64,
    pub files_skipped: u64,
    pub files_rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_saved: u64,
}

#[derive(Debug, Serialize)]
pub struct RootJson {
    pub root: String,
    pub files_seen: u64,
    pub changed: u64,
    pub skipped: u64,
    pub rejected: u64,
    pub errors: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_saved: u64,
}

#[derive(Debug, Serialize)]
pub struct ChangeJson {
    pub path: String,
    pub action: String,
    pub size_before: u64,
    pub size_after: u64,
    pub saved: u64,
    pub quality: u8,
    pub ssimu2: Option<f64>,
}

/// Write `run-<id>.json` into the report directory and return its path.
pub fn write_run_file(
    report: &RunReport,
    dir: &Path,
    host: &str,
    mode: &str,
    engines: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating report directory {}", dir.display()))?;
    let file = RunFile {
        tool_version: VERSION.to_string(),
        run_id: report.run_id,
        started: Utc::now().to_rfc3339(),
        host: host.to_string(),
        mode: mode.to_string(),
        stopped: report.stopped.map(|s| s.to_string()),
        totals: Totals {
            files_seen: report.totals.files_seen,
            files_changed: report.totals.files_changed,
            files_skipped: report.totals.files_skipped,
            files_rejected: report.totals.files_rejected,
            errors: report.totals.errors,
            bytes_before: report.totals.bytes_before,
            bytes_after: report.totals.bytes_after,
            bytes_saved: report.totals.bytes_saved(),
        },
        per_root: report
            .per_root
            .iter()
            .map(|r| RootJson {
                root: r.root.display().to_string(),
                files_seen: r.files_seen,
                changed: r.changed,
                skipped: r.skipped,
                rejected: r.rejected,
                errors: r.errors,
                bytes_before: r.bytes_before,
                bytes_after: r.bytes_after,
                bytes_saved: r.bytes_saved(),
            })
            .collect(),
        top_savings: report
            .top_savings
            .iter()
            .take(20)
            .map(|c| ChangeJson {
                path: c.path.display().to_string(),
                action: c.action.clone(),
                size_before: c.size_before,
                size_after: c.size_after.unwrap_or(c.size_before),
                saved: c
                    .size_before
                    .saturating_sub(c.size_after.unwrap_or(c.size_before)),
                quality: c.quality,
                ssimu2: c.ssimu2,
            })
            .collect(),
        engines: engines.to_string(),
    };
    let path = dir.join(format!("run-{:06}.json", report.run_id));
    let json = serde_json::to_string_pretty(&file)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The single line that goes to syslog/journal, so `monit` and `grep` stay happy.
pub fn summary_line(report: &RunReport) -> String {
    if report.dry_run {
        let planned_files: u64 = report.per_root.iter().map(|r| r.planned_change).sum();
        let before: u64 = report.per_root.iter().map(|r| r.planned_bytes_before).sum();
        let after: u64 = report.per_root.iter().map(|r| r.planned_bytes_after).sum();
        return format!(
            "mediaopt dry-run run={} seen={} would_change={} would_free={} rejected={} errors={}",
            report.run_id,
            report.totals.files_seen,
            planned_files,
            human_bytes(before.saturating_sub(after)),
            report.totals.files_rejected,
            report.totals.errors
        );
    }
    let t = &report.totals;
    let mut line = format!(
        "mediaopt run={} seen={} changed={} skipped={} rejected={} errors={} freed={}",
        report.run_id,
        t.files_seen,
        t.files_changed,
        t.files_skipped,
        t.files_rejected,
        t.errors,
        human_bytes(t.bytes_saved())
    );
    if let Some(stopped) = report.stopped {
        line.push_str(&format!(" stopped={stopped}"));
    }
    line
}

/// Human-readable multi-line summary.
pub fn human_summary(report: &RunReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("\nmediaopt {} - run {}\n", VERSION, report.run_id));
    let dry = report.dry_run;
    let change_col = if dry { "would" } else { "changed" };
    out.push_str(&format!(
        "{:<52} {:>9} {:>9} {:>9} {:>8} {:>7}\n",
        "root", "seen", change_col, "saved", "rejected", "errors"
    ));
    for root in &report.per_root {
        let (changed, saved) = if dry {
            (
                root.planned_change,
                root.planned_bytes_before
                    .saturating_sub(root.planned_bytes_after),
            )
        } else {
            (root.changed, root.bytes_saved())
        };
        out.push_str(&format!(
            "{:<52} {:>9} {:>9} {:>9} {:>8} {:>7}\n",
            truncate(&root.root.display().to_string(), 52),
            root.files_seen,
            changed,
            human_bytes(saved),
            root.rejected,
            root.errors
        ));
    }
    let t = &report.totals;
    let planned_before: u64 = report.per_root.iter().map(|r| r.planned_bytes_before).sum();
    let planned_after: u64 = report.per_root.iter().map(|r| r.planned_bytes_after).sum();
    let planned_files: u64 = report.per_root.iter().map(|r| r.planned_change).sum();
    let (total_changed, total_saved) = if dry {
        (planned_files, planned_before.saturating_sub(planned_after))
    } else {
        (t.files_changed, t.bytes_saved())
    };
    out.push_str(&format!(
        "{:<52} {:>9} {:>9} {:>9} {:>8} {:>7}\n",
        "total",
        t.files_seen,
        total_changed,
        human_bytes(total_saved),
        t.files_rejected,
        t.errors
    ));
    if dry {
        out.push_str("\n(dry run: nothing was written; the figures are measured by encoding into a temporary file)\n");
    }
    if !report.top_savings.is_empty() {
        out.push_str("\nlargest wins this run:\n");
        for change in report.top_savings.iter().take(10) {
            let after = change.size_after.unwrap_or(change.size_before);
            out.push_str(&format!(
                "  {:>9}  {} -> {}{}\n",
                human_bytes(change.size_before.saturating_sub(after)),
                human_bytes(change.size_before),
                human_bytes(after),
                change
                    .ssimu2
                    .map(|s| format!("  (ssimu2 {s:.1})"))
                    .unwrap_or_default()
            ));
            out.push_str(&format!("             {}\n", change.path.display()));
        }
    }
    if let Some(stopped) = report.stopped {
        out.push_str(&format!(
            "\nstopped early: {stopped} (the rest runs next time)\n"
        ));
    }
    out
}

/// Lifetime totals per root, from the manifest.
pub fn lifetime_report(manifest: &Manifest) -> Result<String> {
    let roots = manifest.root_summaries()?;
    let mut out = String::new();
    out.push_str(&format!(
        "{:<52} {:>8} {:>8} {:>8} {:>9}\n",
        "root", "changed", "rejected", "errors", "saved"
    ));
    let mut total = 0u64;
    for RootSummary {
        root,
        changed,
        rejected,
        skipped: _,
        errors,
        bytes_before,
        bytes_after,
    } in roots
    {
        total += bytes_before.saturating_sub(bytes_after);
        out.push_str(&format!(
            "{:<52} {:>8} {:>8} {:>8} {:>9}\n",
            truncate(&root.display().to_string(), 52),
            changed,
            rejected,
            errors,
            human_bytes(bytes_before.saturating_sub(bytes_after))
        ));
    }
    out.push_str(&format!(
        "\n{:<52} {:>8} {:>8} {:>8} {:>9}\n",
        "total",
        "",
        "",
        "",
        human_bytes(total)
    ));
    if let Some(last) = manifest.last_run()? {
        out.push_str(&format!("\nlast run: #{} at {}\n", last.id, last.started));
    }
    Ok(out)
}

/// Delete report files older than `days`.
pub fn prune(dir: &Path, days: u32) -> Result<usize> {
    if days == 0 || !dir.is_dir() {
        return Ok(0);
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86_400);
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if let Ok(modified) = meta.modified() {
            if modified < cutoff && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Pre-formatted line for `mediaopt report --runs`.
pub fn run_line(summary: &RunSummary) -> String {
    format!(
        "#{:<5} {:<22} changed={:<5} freed={:<10} rejected={:<5} errors={}\n",
        summary.id,
        summary.started.chars().take(19).collect::<String>(),
        summary.files_changed,
        human_bytes(summary.bytes_saved()),
        summary.files_rejected,
        summary.errors
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .rev()
            .take(max.saturating_sub(3))
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("...{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileRecord;
    use crate::run::RootReport;

    fn sample_report() -> RunReport {
        RunReport {
            run_id: 7,
            dry_run: false,
            totals: crate::manifest::RunTotals {
                files_seen: 100,
                files_changed: 10,
                files_skipped: 85,
                files_rejected: 4,
                errors: 1,
                bytes_before: 10_000_000,
                bytes_after: 4_000_000,
            },
            stopped: None,
            per_root: vec![RootReport {
                root: PathBuf::from("/home/example/www"),
                files_seen: 100,
                changed: 10,
                skipped: 85,
                rejected: 4,
                errors: 1,
                bytes_before: 10_000_000,
                bytes_after: 4_000_000,
                planned_change: 0,
                planned_bytes_before: 0,
                planned_bytes_after: 0,
            }],
            top_savings: vec![FileRecord {
                path: PathBuf::from("/home/example/www/a.jpg"),
                action: "jpeg_reencode".into(),
                size_before: 900_000,
                size_after: Some(300_000),
                quality: 82,
                ssimu2: Some(78.4),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn summary_line_is_one_line_and_greppable() {
        let line = summary_line(&sample_report());
        assert!(!line.contains('\n'));
        assert!(line.contains("changed=10"));
        assert!(line.contains("freed=5.72 MiB"));
    }

    #[test]
    fn human_summary_mentions_totals_and_wins() {
        let out = human_summary(&sample_report());
        assert!(out.contains("total"));
        assert!(out.contains("/home/example/www/a.jpg"));
        assert!(out.contains("ssimu2 78.4"));
    }

    #[test]
    fn run_file_is_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_run_file(&sample_report(), dir.path(), "web-a", "apply", "vips 8.18").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["totals"]["bytes_saved"], 6_000_000);
        assert_eq!(value["per_root"][0]["changed"], 10);
    }

    #[test]
    fn truncation_keeps_the_tail() {
        assert_eq!(truncate("short", 10), "short");
        let long = "/very/long/path/that/keeps/going/and/going/2024/05/photo.jpg";
        let t = truncate(long, 20);
        assert!(t.chars().count() <= 20);
        assert!(t.ends_with("photo.jpg"));
    }
}
