//! Configuration: one TOML file per host, a list of roots with per-root policy.
//!
//! The file is intentionally flat and boring: it must stay readable by a human
//! five years from now, and it must be safe to keep in `/etc` on a server while
//! the repository only carries `config.example.toml` with placeholder paths.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// How the encoder picks JPEG quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityMode {
    /// Binary-search the lowest quality that still meets `ssimu2_floor`.
    /// Slower (2-4 encodes per file) but it never drops below the floor and
    /// never wastes bytes on files that reach the floor at a lower setting.
    #[default]
    Target,
    /// One encode at `fixed_quality`. Fast and predictable.
    Fixed,
}

/// Chroma subsampling policy for re-encoded JPEGs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubsamplePolicy {
    /// Follow the source: keep 4:4:4 if the source is 4:4:4, else use 4:2:0.
    #[default]
    Auto,
    /// Always 4:2:0 (smallest, right for photographs).
    On,
    /// Always 4:4:4 (largest, right for graphics and screenshots).
    Off,
}

/// What to do with PNG files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PngMode {
    /// Never touch PNG files.
    Skip,
    /// Recompress losslessly only (`oxipng`). Measured gain on already-optimised
    /// photo PNGs is 0.0-3%; keep it because it is free and never worse.
    Lossless,
    /// Palette-quantise when libimagequant meets `quality_floor`, else fall back
    /// to the lossless path. Measured 53-68% on opaque screenshots and photos,
    /// but visible banding is possible, hence the quality floor.
    #[default]
    QuantIfSafe,
}

/// Per-root policy. Roots are allowed to be more conservative than the global
/// settings, never more aggressive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPolicy {
    /// Full pipeline.
    #[default]
    Optimise,
    /// Lossless only: no re-encode, no resize, no palette quantisation, and
    /// metadata is always preserved. For hand-finished photography.
    LosslessOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpegConfig {
    #[serde(default)]
    pub quality_mode: QualityMode,
    /// SSIMULACRA2 floor in target mode. 75 is "high quality" on the published
    /// scale (70 = high, 90 = indistinguishable at 1:1) and matches a good
    /// mozjpeg q85; 78-80 is conservative.
    #[serde(default = "d_ssimu2_floor")]
    pub ssimu2_floor: f64,
    /// Quality used in fixed mode.
    #[serde(default = "d_fixed_quality")]
    pub fixed_quality: u8,
    /// Only re-encode lossily when the file's stored quality is at least this.
    /// Everything below gets the lossless pass instead. This is also what
    /// guarantees we never re-encode our own q85 output a second time.
    #[serde(default = "d_reencode_min_quality")]
    pub reencode_min_quality: u8,
    /// Longest side after resizing. 0 disables resizing.
    #[serde(default = "d_max_dimension")]
    pub max_dimension: u32,
    /// libvips kernel name for downscaling. `cubic` is Catmull-Rom, which
    /// measured 4.1% smaller than lanczos3 with no ringing.
    #[serde(default = "d_kernel")]
    pub resize_kernel: String,
    /// A lossy re-encode is only kept when the result is at least this much
    /// smaller. Guards against anomalies (unusual quantisation tables) where a
    /// small gain is not worth the generation loss.
    #[serde(default = "d_min_gain_lossy")]
    pub min_gain_percent: f64,
    /// Run `jpegtran -optimize -progressive` on JPEGs that are not re-encoded.
    /// Measured 4.8% on an already-stripped production tree, up to 8.2% per file.
    #[serde(default = "d_true")]
    pub lossless_pass: bool,
    /// Minimum gain for the lossless pass. Any real win counts.
    #[serde(default = "d_min_gain_lossless")]
    pub min_gain_percent_lossless: f64,
    #[serde(default)]
    pub subsample: SubsamplePolicy,
    /// Metadata segments to keep. `["icc", "exif"]` is the safe minimum.
    #[serde(default = "d_keep")]
    pub keep: Vec<String>,
}

impl Default for JpegConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty jpeg config must parse")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PngConfig {
    #[serde(default)]
    pub mode: PngMode,
    /// libimagequant quality floor (0-100). Below this the palette result is
    /// discarded and the lossless path is used instead.
    #[serde(default = "d_png_quality_floor")]
    pub quality_floor: u8,
    /// Only palette-quantise when the result is at least this much smaller.
    #[serde(default = "d_min_gain_lossy")]
    pub min_gain_percent: f64,
    /// oxipng effort level for the lossless path (0-6; 2-4 is the sweet spot).
    #[serde(default = "d_oxipng_effort")]
    pub lossless_effort: u8,
}

impl Default for PngConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty png config must parse")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebpConfig {
    /// Re-pack lossless WebP files only. Lossy WebP is never re-encoded: there
    /// is no way to shrink it without a second lossy generation.
    #[serde(default = "d_true")]
    pub lossless_only: bool,
}

impl Default for WebpConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty webp config must parse")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Worker threads. The tool also sets nice 19 and idle I/O on itself.
    #[serde(default = "d_jobs")]
    pub jobs: usize,
    /// Hard stop after this long; the run finishes the file in flight and exits.
    #[serde(default = "d_max_runtime")]
    pub max_runtime: String,
    /// Files modified more recently than this are skipped (an upload in progress).
    #[serde(default = "d_min_file_age")]
    pub min_file_age: String,
    /// Refuse to start below this much free space on the target filesystem.
    #[serde(default = "d_free_space_reserve")]
    pub free_space_reserve: String,
    /// Files smaller than this are never considered (savings are noise).
    #[serde(default = "d_min_file_size")]
    pub min_file_size: u64,
    /// Byte budget for one run; 0 = unlimited. Keeps the first pass staged so a
    /// mistake is caught inside the nightly backup window.
    #[serde(default)]
    pub max_bytes_per_run: u64,
    /// File budget for one run; 0 = unlimited.
    #[serde(default)]
    pub max_files_per_run: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty limits config must parse")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// SQLite manifest. Never inside a media tree: those get served and backed up.
    #[serde(default = "d_db_path")]
    pub db: PathBuf,
    /// Directory for per-run JSON reports.
    #[serde(default = "d_report_dir")]
    pub report_dir: PathBuf,
    /// Optional log file (the container usually logs to stdout instead).
    #[serde(default)]
    pub log_file: Option<PathBuf>,
    /// Days of JSON reports to keep.
    #[serde(default = "d_report_days")]
    pub report_days: u32,
}

impl Default for StateConfig {
    fn default() -> Self {
        toml::from_str("").expect("empty state config must parse")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub path: PathBuf,
    /// `user:group` applied to files this tool replaces. When omitted the
    /// original owner and group of each file are preserved, which is almost
    /// always what you want.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub policy: RootPolicy,
    /// Only used for roots of `policy = "lossless_only"`: keep these metadata
    /// classes. Defaults to ICC + EXIF.
    #[serde(default = "d_keep")]
    pub keep: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
    /// Managed by `mediaopt remote` from another machine; the container skips it.
    #[serde(default)]
    pub managed_by_remote: bool,
    /// Per-root budget overrides.
    #[serde(default)]
    pub max_bytes_per_run: Option<u64>,
    #[serde(default)]
    pub max_files_per_run: Option<u64>,
    /// Glob-ish suffix/substring excludes, matched against the full path.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl RootConfig {
    pub fn keep_or_default(&self) -> &[String] {
        if self.keep.is_empty() {
            &[]
        } else {
            &self.keep
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// libvips binary. Defaults to `$VIPS_BIN` or `vips` on PATH.
    #[serde(default)]
    pub vips: Option<PathBuf>,
    /// mozjpeg `jpegtran` binary. Defaults to `$JPEGTRAN_BIN` or `jpegtran`.
    #[serde(default)]
    pub jpegtran: Option<PathBuf>,
    /// `pngquant` binary for palette quantisation.
    #[serde(default)]
    pub pngquant: Option<PathBuf>,
    /// `ssimulacra2` binary, used for verification in target mode.
    #[serde(default)]
    pub ssimulacra2: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub jpeg: JpegConfig,
    #[serde(default)]
    pub png: PngConfig,
    #[serde(default)]
    pub webp: WebpConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub state: StateConfig,
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub roots: Vec<RootConfig>,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").expect("empty config must parse")
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("cannot parse config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.roots.is_empty() {
            bail!("config defines no [[roots]]; nothing to do");
        }
        if !(60.0..=100.0).contains(&self.jpeg.ssimu2_floor) {
            bail!(
                "jpeg.ssimu2_floor must be between 60 and 100, got {}",
                self.jpeg.ssimu2_floor
            );
        }
        if !(1..=100).contains(&self.jpeg.fixed_quality) {
            bail!("jpeg.fixed_quality must be 1-100");
        }
        if self.jpeg.reencode_min_quality > 100 {
            bail!("jpeg.reencode_min_quality must be 1-100");
        }
        if self.limits.jobs == 0 {
            bail!("limits.jobs must be at least 1");
        }
        for root in &self.roots {
            if !root.path.is_absolute() {
                bail!("root path must be absolute, got {}", root.path.display());
            }
            if let Some(owner) = &root.owner {
                if !owner.contains(':') {
                    bail!("root owner must be user:group, got {owner}");
                }
            }
        }
        Ok(())
    }

    /// Enabled roots that this process should handle.
    pub fn active_roots(&self) -> impl Iterator<Item = &RootConfig> {
        self.roots
            .iter()
            .filter(|r| r.enabled && !r.managed_by_remote)
    }

    pub fn find_root(&self, path: &Path) -> Option<&RootConfig> {
        self.roots.iter().find(|r| path.starts_with(&r.path))
    }

    pub fn report_dir(&self) -> &Path {
        &self.state.report_dir
    }

    /// A digest of everything that can change a decision.
    ///
    /// Stored with every manifest row: when it differs from the current run, the
    /// fast path re-evaluates the file instead of skipping it. Without this,
    /// tightening the quality floor or turning the lossless pass on would have no
    /// effect on files an earlier run had already decided about.
    pub fn decision_digest(&self, root: &RootConfig) -> String {
        use std::fmt::Write as _;
        let j = &self.jpeg;
        let mut text = String::with_capacity(256);
        let _ = write!(
            text,
            "jpeg:{:?}:{}:{}:{}:{}:{}:{}:{}:{}:{:?}:{:?}|png:{:?}:{}:{}:{}|webp:{}|min:{}|root:{:?}:{:?}",
            j.quality_mode,
            j.ssimu2_floor,
            j.fixed_quality,
            j.reencode_min_quality,
            j.max_dimension,
            j.resize_kernel,
            j.min_gain_percent,
            j.lossless_pass,
            j.min_gain_percent_lossless,
            j.subsample,
            j.keep,
            self.png.mode,
            self.png.quality_floor,
            self.png.min_gain_percent,
            self.png.lossless_effort,
            self.webp.lossless_only,
            self.limits.min_file_size,
            root.policy,
            root.keep
        );
        crate::replace::sha256_bytes(text.as_bytes())[..16].to_string()
    }
}

fn d_true() -> bool {
    true
}
fn d_ssimu2_floor() -> f64 {
    75.0
}
fn d_fixed_quality() -> u8 {
    85
}
fn d_reencode_min_quality() -> u8 {
    86
}
fn d_max_dimension() -> u32 {
    2048
}
fn d_kernel() -> String {
    "cubic".to_string()
}
fn d_min_gain_lossy() -> f64 {
    5.0
}
fn d_min_gain_lossless() -> f64 {
    0.5
}
fn d_png_quality_floor() -> u8 {
    80
}
fn d_oxipng_effort() -> u8 {
    2
}
fn d_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(2))
        .unwrap_or(1)
}
fn d_max_runtime() -> String {
    "5h".to_string()
}
fn d_min_file_age() -> String {
    "30m".to_string()
}
fn d_free_space_reserve() -> String {
    "2GiB".to_string()
}
fn d_min_file_size() -> u64 {
    5 * 1024
}
fn d_db_path() -> PathBuf {
    PathBuf::from("/var/lib/mediaopt/state.db")
}
fn d_report_dir() -> PathBuf {
    PathBuf::from("/var/lib/mediaopt/reports")
}
fn d_report_days() -> u32 {
    90
}
fn d_keep() -> Vec<String> {
    vec!["icc".to_string(), "exif".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_sane_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.jpeg.ssimu2_floor, 75.0);
        assert_eq!(cfg.jpeg.fixed_quality, 85);
        assert_eq!(cfg.jpeg.reencode_min_quality, 86);
        assert_eq!(cfg.jpeg.max_dimension, 2048);
        assert_eq!(cfg.png.mode, PngMode::QuantIfSafe);
        assert!(cfg.roots.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("[jpeg]\nqualty = 85\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn roots_round_trip() {
        let text = r#"
[[roots]]
path = "/home/example/www/static/img"
owner = "example:example"
enabled = true

[[roots]]
path = "/home/example/private/photos"
policy = "lossless_only"
keep = ["icc", "exif"]
enabled = true
managed_by_remote = false
exclude = ["/thumbs/"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.active_roots().count(), 2);
        assert_eq!(cfg.roots[1].policy, RootPolicy::LosslessOnly);
    }

    #[test]
    fn relative_root_is_rejected() {
        let cfg: Config = toml::from_str("[[roots]]\npath = \"rel/path\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}
