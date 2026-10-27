//! The encoder layer: libvips for JPEG/PNG, mozjpeg `jpegtran` for lossless
//! JPEG transcoding, `oxipng` for lossless PNG, `pngquant` for palette
//! quantisation, `ssimulacra2` for measurement.
//!
//! Everything is a subprocess. That is a deliberate trade: this tool has to run
//! unchanged on an Apple Silicon laptop and inside a Debian container, and
//! linking libvips *built against mozjpeg* (the only build that reaches mozjpeg
//! density) is far easier to ship as a binary than as a Rust FFI dependency.
//! Measured cost: the CLI path reaches 3.54 images/s against 4.14 in-process,
//! about 15%, against an encode of ~250 ms per 2048px photo.
//!
//! Every command that writes an image writes to a temporary file that the caller
//! renames into place only after verification.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::{EngineConfig, SubsamplePolicy};
use crate::jpeg::Subsampling;

/// Predefined quantisation table index in libvips/mozjpeg.
/// 3 is mozjpeg's default (Robidoux); with any other table libvips produces
/// files 2.9-5.2% larger at the same SSIMULACRA2 score.
const QUANT_TABLE: &str = "3";

#[derive(Debug, Clone)]
pub struct Tool {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, Default)]
pub struct Tools {
    pub vips: Option<Tool>,
    pub jpegtran: Option<Tool>,
    pub pngquant: Option<Tool>,
    pub ssimulacra2: Option<Tool>,
}

impl Tools {
    /// Discover the tools from explicit config paths, else `$<NAME>_BIN`, else PATH.
    pub fn discover(cfg: &EngineConfig) -> Self {
        Tools {
            vips: find(cfg.vips.clone(), "VIPS_BIN", "vips", &["--version"]),
            // mozjpeg's jpegtran only understands the single-dash form.
            jpegtran: find(
                cfg.jpegtran.clone(),
                "JPEGTRAN_BIN",
                "jpegtran",
                &["-version"],
            ),
            pngquant: find(
                cfg.pngquant.clone(),
                "PNGQUANT_BIN",
                "pngquant",
                &["--version"],
            ),
            ssimulacra2: find(
                cfg.ssimulacra2.clone(),
                "SSIMULACRA2_BIN",
                "ssimulacra2",
                &[],
            ),
        }
    }

    pub fn missing_required(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.vips.is_none() {
            missing.push("vips");
        }
        if self.jpegtran.is_none() {
            missing.push("jpegtran");
        }
        missing
    }

    pub fn describe(&self) -> String {
        let show = |name: &str, t: &Option<Tool>| match t {
            Some(t) => format!("{name} {} ({})", t.version, t.path.display()),
            None => format!("{name} MISSING"),
        };
        [
            show("vips", &self.vips),
            show("jpegtran", &self.jpegtran),
            show("pngquant", &self.pngquant),
            show("ssimulacra2", &self.ssimulacra2),
        ]
        .join(", ")
    }

    fn vips(&self) -> Result<&Tool> {
        self.vips
            .as_ref()
            .context("libvips (`vips`) not found; set engine.vips or VIPS_BIN")
    }

    fn jpegtran(&self) -> Result<&Tool> {
        self.jpegtran
            .as_ref()
            .context("mozjpeg `jpegtran` not found; set engine.jpegtran or JPEGTRAN_BIN")
    }
}

fn find(explicit: Option<PathBuf>, env: &str, name: &str, version_args: &[&str]) -> Option<Tool> {
    let path = explicit
        .or_else(|| std::env::var_os(env).map(PathBuf::from))
        .or_else(|| which(name))?;
    match tool_version(&path, version_args) {
        Some(version) => Some(Tool { path, version }),
        None => {
            // The file is there but cannot run (wrong architecture, missing shared
            // library, bad extraction). Treat it as absent so the run fails loudly
            // instead of encoding garbage.
            tracing::warn!("{} exists but cannot run", path.display());
            None
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Probe a tool. Returns `None` when the binary cannot run at all, which is a
/// configuration error we must not paper over.
fn tool_version(path: &Path, args: &[&str]) -> Option<String> {
    if args.is_empty() {
        // The tool has no version flag (ssimulacra2). Just prove it runs.
        let out = command(path).output().ok()?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if combined.contains("Symbol not found") || combined.contains("Library not loaded") {
            return None;
        }
        return Some("present".to_string());
    }
    let out = command(path).args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = format!("{stdout}{stderr}");
    if combined.contains("Symbol not found")
        || combined.contains("Library not loaded")
        || combined.contains("error while loading shared libraries")
        || combined.contains("Exec format error")
    {
        return None;
    }
    // Prefer a line that looks like a version number; fall back to the first
    // non-empty line, which for tools without --version is a usage banner.
    let version = combined
        .lines()
        .map(|l| l.trim())
        .find(|l| looks_like_version(l))
        .or_else(|| combined.lines().map(|l| l.trim()).find(|l| !l.is_empty()))
        .unwrap_or("unknown")
        .to_string();
    Some(version)
}

fn looks_like_version(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
}

fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    cmd
}

#[derive(Debug, Clone)]
pub struct JpegEncodeOpts {
    pub quality: u8,
    /// Downscale so the longest side becomes this, when set.
    pub resize_to: Option<u32>,
    /// Longest side of the source, needed to compute the resize scale.
    pub source_longest: u32,
    /// Stored quality of the source, used to start the quality search.
    pub source_quality: u8,
    pub subsample: SubsamplePolicy,
    pub source_subsampling: Subsampling,
    pub kernel: String,
    pub keep: Vec<String>,
}

impl JpegEncodeOpts {
    fn subsample_mode(&self) -> &'static str {
        match self.subsample {
            SubsamplePolicy::On => "on",
            SubsamplePolicy::Off => "off",
            SubsamplePolicy::Auto => {
                // Keep 4:4:4 for images that are already 4:4:4: those are
                // graphics and screenshots where 4:2:0 smears coloured edges.
                if self.source_subsampling == Subsampling::None {
                    "off"
                } else {
                    "on"
                }
            }
        }
    }

    fn keep_string(&self) -> String {
        if self.keep.is_empty() {
            "none".to_string()
        } else {
            self.keep.join(",")
        }
    }

    /// The saver options, both as CLI flags and as bracket arguments.
    fn saver_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("Q".into(), self.quality.to_string()),
            ("optimize-coding".into(), "true".into()),
            ("interlace".into(), "true".into()),
            ("trellis-quant".into(), "true".into()),
            ("overshoot-deringing".into(), "true".into()),
            ("optimize-scans".into(), "true".into()),
            ("quant-table".into(), QUANT_TABLE.into()),
            ("subsample-mode".into(), self.subsample_mode().into()),
            ("keep".into(), self.keep_string()),
        ]
    }
}

/// Saver options that are plain flags rather than key/value pairs.
const SAVER_FLAGS: [&str; 5] = [
    "optimize-coding",
    "interlace",
    "trellis-quant",
    "overshoot-deringing",
    "optimize-scans",
];

/// Encode a JPEG, optionally downscaling. Writes to `dst` (a temporary file).
///
/// Arguments are built as `OsString`, never as `String`: a real tree contains file
/// names that are not valid UTF-8, and `display()` would rewrite those bytes into
/// U+FFFD, at which point libvips reports that the input does not exist.
///
/// The resize case is two explicit steps instead of the `output.jpg[options]`
/// shorthand. That shorthand cannot express `keep=icc,exif`, because libvips splits
/// the option string on commas and then treats `exif` as a filename, so the resize
/// path silently used different metadata rules from the plain re-encode. Two
/// commands also mean one definition of the saver options for both paths.
pub fn encode_jpeg(tools: &Tools, src: &Path, dst: &Path, o: &JpegEncodeOpts) -> Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let vips = tools.vips()?;

    // argv for `vips jpegsave <in> <out>`, with the density-critical flags.
    let jpegsave_args = |input: &Path, output: &Path| -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        args.push(OsString::from("jpegsave"));
        args.push(input.as_os_str().to_os_string());
        args.push(output.as_os_str().to_os_string());
        for (key, value) in o.saver_pairs() {
            args.push(OsString::from(format!("--{key}")));
            if !SAVER_FLAGS.contains(&key.as_str()) {
                args.push(OsString::from(value));
            }
        }
        args
    };

    let with_suffix = |path: &Path, suffix: &str| -> std::path::PathBuf {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.extend_from_slice(suffix.as_bytes());
        OsString::from_vec(bytes).into()
    };

    match o.resize_to {
        Some(target) if o.source_longest > target => {
            let scale = target as f64 / o.source_longest as f64;
            // libvips picks the saver from the output extension, so the final JPEG
            // must be written to a name it recognises; the intermediate is a native
            // vips image so no metadata is lost between the two steps.
            let intermediate = with_suffix(dst, ".part.v");
            let encoded = with_suffix(dst, ".part.jpg");

            let mut resize_cmd = command(&vips.path);
            resize_cmd
                .arg("resize")
                .arg(src)
                .arg(&intermediate)
                .arg(format!("{scale:.8}"))
                .arg("--kernel")
                .arg(&o.kernel);
            if let Err(e) = run(resize_cmd, "libvips resize")
                .with_context(|| format!("resizing {}", src.display()))
            {
                let _ = std::fs::remove_file(&intermediate);
                return Err(e);
            }

            let mut save_cmd = command(&vips.path);
            save_cmd.args(jpegsave_args(&intermediate, &encoded));
            let saved = run(save_cmd, "libvips JPEG encode")
                .with_context(|| format!("encoding {}", src.display()));
            let _ = std::fs::remove_file(&intermediate);
            saved?;

            if let Err(e) = std::fs::rename(&encoded, dst) {
                let _ = std::fs::remove_file(&encoded);
                return Err(e).with_context(|| {
                    format!("moving {} onto {}", encoded.display(), dst.display())
                });
            }
        }
        _ => {
            let mut cmd = command(&vips.path);
            cmd.args(jpegsave_args(src, dst));
            run(cmd, "libvips JPEG encode")
                .with_context(|| format!("encoding {}", src.display()))?;
        }
    }
    Ok(())
}

/// Lossless JPEG transcode: progressive + optimal Huffman tables, all metadata
/// copied verbatim. Pixels are untouched, so this can never lose quality.
pub fn lossless_jpeg(tools: &Tools, src: &Path, dst: &Path) -> Result<()> {
    let jpegtran = tools.jpegtran()?;
    let mut cmd = command(&jpegtran.path);
    cmd.arg("-copy")
        .arg("all")
        .arg("-optimize")
        .arg("-progressive")
        .arg("-outfile")
        .arg(dst)
        .arg(src);
    run(cmd, "jpegtran")?;
    Ok(())
}

/// Palette-quantise a PNG. Returns `false` when pngquant declined because it
/// could not meet the quality floor (exit status 99) - the caller then falls
/// back to the lossless path.
pub fn quantise_png(
    tools: &Tools,
    src: &Path,
    dst: &Path,
    quality_floor: u8,
    speed: u8,
) -> Result<bool> {
    let Some(pngquant) = &tools.pngquant else {
        bail!("pngquant not found; set engine.pngquant or PNGQUANT_BIN");
    };
    let mut cmd = command(&pngquant.path);
    cmd.arg("--quality")
        .arg(format!("{quality_floor}-100"))
        .arg("--speed")
        .arg(speed.to_string())
        .arg("--force")
        // Explicit so behaviour is identical on macOS (where pngquant strips by
        // default) and Linux. The palette path is only used for images without
        // an ICC profile, so nothing important is lost.
        .arg("--strip")
        .arg("--output")
        .arg(dst)
        .arg(src);
    let output = cmd.output().context("running pngquant")?;
    match output.status.code() {
        Some(0) => Ok(true),
        // 99: conversion would lose too much quality; 98: too few colours.
        Some(98) | Some(99) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("pngquant failed ({}): {}", output.status, stderr.trim())
        }
    }
}

/// Lossless PNG recompression with oxipng, run in-process.
pub fn lossless_png(src: &Path, dst: &Path, effort: u8) -> Result<()> {
    let data = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let mut opts = oxipng::Options::from_preset(effort.min(6));
    // Never strip: this path is also used for hand-finished photography.
    opts.strip = oxipng::StripChunks::None;
    let optimised = oxipng::optimize_from_memory(&data, &opts)
        .with_context(|| format!("oxipng on {}", src.display()))?;
    std::fs::write(dst, optimised).with_context(|| format!("writing {}", dst.display()))?;
    Ok(())
}

/// SSIMULACRA2 score of `distorted` against `orig` (higher is better, 100 is
/// identical). Returns `None` when the binary is unavailable.
pub fn ssimu2(tools: &Tools, orig: &Path, distorted: &Path) -> Result<Option<f64>> {
    let Some(tool) = &tools.ssimulacra2 else {
        return Ok(None);
    };
    let output = command(&tool.path)
        .arg(orig)
        .arg(distorted)
        .output()
        .context("running ssimulacra2")?;
    if !output.status.success() {
        // Some images cannot be measured at all: libjxl refuses when an embedded
        // ICC profile contradicts the colour type, for example. That must not cost
        // the file its optimisation, so the caller gets "no score" and falls back
        // to the fixed quality.
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn_metric_failure_once(stderr.trim());
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let score = text
        .split_whitespace()
        .last()
        .and_then(|s| s.parse::<f64>().ok());
    Ok(score)
}

/// Read a header field through libvips (`width`, `height`, `bands`).
pub fn header_field(tools: &Tools, path: &Path, field: &str) -> Result<Option<String>> {
    let vips = tools.vips()?;
    let vipsheader = vips.path.with_file_name("vipsheader");
    let program = if vipsheader.is_file() {
        vipsheader
    } else {
        which("vipsheader").context("vipsheader not found next to vips or on PATH")?
    };
    let output = command(&program)
        .arg("-f")
        .arg(field)
        .arg(path)
        .output()
        .context("running vipsheader")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Verify that an encoded file decodes and has the expected dimensions.
/// This is the gate that stops a broken encode from ever replacing a good file.
pub fn verify_decodes(
    tools: &Tools,
    path: &Path,
    expect: Option<(u32, u32)>,
) -> Result<(u32, u32)> {
    let width = header_field(tools, path, "width")?
        .and_then(|v| v.parse::<u32>().ok())
        .with_context(|| format!("output {} does not decode", path.display()))?;
    let height = header_field(tools, path, "height")?
        .and_then(|v| v.parse::<u32>().ok())
        .with_context(|| format!("output {} has no height", path.display()))?;
    if width == 0 || height == 0 {
        bail!("output {} decoded to {}x{}", path.display(), width, height);
    }
    if let Some((ew, eh)) = expect {
        // Allow a one-pixel rounding difference from the scaler.
        if width.abs_diff(ew) > 1 || height.abs_diff(eh) > 1 {
            bail!(
                "output {} is {}x{}, expected {}x{}",
                path.display(),
                width,
                height,
                ew,
                eh
            );
        }
    }
    Ok((width, height))
}

/// Once per process, so a systemic failure is visible without one line per file.
fn warn_metric_failure_once(detail: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "ssimulacra2 could not measure an image ({detail}); files it cannot measure are \
             encoded at the configured quality without a floor check"
        );
    } else {
        tracing::debug!("ssimulacra2 could not measure an image: {detail}");
    }
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().to_string();
    let output = cmd
        .output()
        .with_context(|| format!("spawning {program} for {what}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{what} failed ({}): {}",
            output.status,
            stderr
                .trim()
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    Ok(())
}

/// Compute the output dimensions for a downscale to `target` on the longest side.
pub fn scaled_dimensions(width: u32, height: u32, target: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let longest = width.max(height);
    if longest <= target {
        return (width, height);
    }
    let scale = target as f64 / longest as f64;
    let w = (width as f64 * scale).round().max(1.0) as u32;
    let h = (height as f64 * scale).round().max(1.0) as u32;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(subsample: SubsamplePolicy, src_sub: Subsampling) -> JpegEncodeOpts {
        JpegEncodeOpts {
            quality: 85,
            resize_to: None,
            source_longest: 2048,
            source_quality: 90,
            subsample,
            source_subsampling: src_sub,
            kernel: "cubic".into(),
            keep: vec!["icc".into(), "exif".into()],
        }
    }

    #[test]
    fn auto_subsample_follows_the_source() {
        assert_eq!(
            opts(SubsamplePolicy::Auto, Subsampling::H2V2).subsample_mode(),
            "on"
        );
        assert_eq!(
            opts(SubsamplePolicy::Auto, Subsampling::None).subsample_mode(),
            "off",
            "4:4:4 sources must stay 4:4:4"
        );
        assert_eq!(
            opts(SubsamplePolicy::On, Subsampling::None).subsample_mode(),
            "on"
        );
    }

    #[test]
    fn saver_options_include_the_density_critical_set() {
        let pairs = opts(SubsamplePolicy::On, Subsampling::H2V2).saver_pairs();
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        for required in [
            "Q",
            "optimize-coding",
            "interlace",
            "trellis-quant",
            "overshoot-deringing",
            "optimize-scans",
            "quant-table",
            "subsample-mode",
            "keep",
        ] {
            assert!(keys.contains(&required), "missing saver option {required}");
        }
        let quant = pairs.iter().find(|(k, _)| k == "quant-table").unwrap();
        assert_eq!(quant.1, "3", "without table 3 libvips is 2.9-5.2% fatter");
    }

    #[test]
    fn empty_keep_becomes_none() {
        let mut o = opts(SubsamplePolicy::On, Subsampling::H2V2);
        o.keep = vec![];
        assert_eq!(o.keep_string(), "none");
    }

    #[test]
    fn scaling_keeps_aspect_ratio() {
        assert_eq!(scaled_dimensions(4080, 2296, 2048), (2048, 1153));
        assert_eq!(scaled_dimensions(1790, 4080, 2048), (899, 2048));
        assert_eq!(scaled_dimensions(1200, 800, 2048), (1200, 800));
        assert_eq!(scaled_dimensions(100, 100, 2048), (100, 100));
    }
}
