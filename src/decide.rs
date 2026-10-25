//! The decision engine: one function that turns a probed file into exactly one
//! action, with a stable machine-readable reason.
//!
//! This is the part of the tool that must stay correct for years, so every rule
//! is explicit and covered by a test. The rules encode the measured behaviour of
//! the corpus:
//!
//! * re-encoding a JPEG that is already at or below q85 gains ~0% and costs a
//!   generation of quality, so it is never done;
//! * 4.8% is still available on those files **losslessly** (progressive +
//!   Huffman + metadata), so they get that instead;
//! * only JPEGs at q86 and above are re-encoded. With `quality_mode = "fixed"`
//!   at q85 that also means our own output sits below the threshold. With
//!   `quality_mode = "target"` the search can legitimately pick a quality above
//!   86 to hold the perceptual floor, so idempotency there rests on the manifest
//!   fingerprint and the in-file COM marker, not on the threshold alone.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, RootConfig, RootPolicy};
use crate::probe::{Content, Format};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Re-encode and/or resize with the JPEG engine.
    JpegReencode,
    /// Lossless JPEG transcode (`jpegtran -optimize -progressive`).
    JpegLossless,
    /// Palette quantisation via libimagequant.
    PngQuant,
    /// Lossless PNG recompression.
    PngLossless,
    /// Lossless WebP re-pack.
    WebpLossless,
    /// Nothing to do; `reason` says why. Recorded so the next run does not
    /// reconsider the file.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Smaller than `limits.min_file_size`.
    TooSmall,
    /// Modified too recently; an upload may still be finishing.
    TooNew,
    ZeroBytes,
    /// GIF, AVIF, HEIC, TIFF, BMP, JXL or unreadable bytes (not in v1 scope).
    FormatNotHandled,
    /// Header could not be parsed; do not touch a file we do not understand.
    UnparsableHeader,
    /// 4-component JPEG (CMYK/YCCK): browsers and our encoder disagree on colour.
    Cmyk,
    /// Already lean: q85 or below, so a lossy re-encode is not worth it.
    QualityAlreadyLean,
    /// Longest side is already within `jpeg.max_dimension`.
    DimensionOk,
    /// Palette quantisation was skipped because the PNG has alpha or a palette.
    PngHasAlpha,
    /// Lossy WebP: re-encoding would be a second lossy generation.
    WebpLossy,
    /// PNG handling disabled in the configuration for this root.
    FormatDisabled,
    /// The manifest already recorded that no gain is achievable.
    NoGainKnown,
    /// `policy = "lossless_only"` and the lossless pass is disabled.
    LosslessDisabled,
    /// The file carries this tool's COM marker: it is our own output, even if the
    /// manifest has no row for it (restore, rebuild, new server).
    AlreadyProcessed,
    /// PNG with an embedded ICC profile: palette quantisation would drop it, so
    /// the lossless path is used instead.
    PngHasIcc,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::TooSmall => "too small to be worth it",
            Reason::TooNew => "modified too recently",
            Reason::ZeroBytes => "zero-byte file",
            Reason::FormatNotHandled => "format not handled in v1",
            Reason::UnparsableHeader => "header did not parse",
            Reason::Cmyk => "CMYK JPEG",
            Reason::QualityAlreadyLean => "already at q85 or below",
            Reason::DimensionOk => "already within the dimension limit",
            Reason::PngHasAlpha => "PNG has alpha or a palette",
            Reason::WebpLossy => "lossy WebP",
            Reason::FormatDisabled => "format disabled in config",
            Reason::NoGainKnown => "no gain recorded by an earlier run",
            Reason::LosslessDisabled => "lossless pass disabled",
            Reason::AlreadyProcessed => "already processed by this tool",
            Reason::PngHasIcc => "PNG carries an ICC profile",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    pub reason: Option<Reason>,
    /// Set when the file must be downscaled to this longest side.
    pub resize_to: Option<u32>,
    /// The stored quality the decision was based on (0 when unknown).
    pub stored_quality: u8,
    /// True when the source's quantisation tables are not IJG-scaled, so the
    /// quality estimate is approximate.
    pub quality_approximate: bool,
}

impl Decision {
    fn skip(reason: Reason, stored_quality: u8) -> Self {
        Decision {
            action: Action::Skip,
            reason: Some(reason),
            resize_to: None,
            stored_quality,
            quality_approximate: false,
        }
    }

    pub fn is_skip(&self) -> bool {
        self.action == Action::Skip
    }
}

/// Everything the decision needs that is not already in the config.
pub struct Facts<'a> {
    pub content: &'a Content,
    pub size: u64,
    pub age: Duration,
}

pub fn decide(facts: &Facts<'_>, cfg: &Config, root: &RootConfig) -> Decision {
    if facts.size == 0 {
        return Decision::skip(Reason::ZeroBytes, 0);
    }
    if facts.size < cfg.limits.min_file_size {
        return Decision::skip(Reason::TooSmall, 0);
    }
    let min_age = parse_duration(&cfg.limits.min_file_age).unwrap_or_default();
    if !min_age.is_zero() && facts.age < min_age {
        return Decision::skip(Reason::TooNew, 0);
    }

    let content = facts.content;
    match content.format() {
        Format::Jpeg => decide_jpeg(content, cfg, root),
        Format::Png => decide_png(content, cfg, root),
        Format::Webp => decide_webp(content, cfg),
        _ => Decision::skip(Reason::FormatNotHandled, 0),
    }
}

fn decide_jpeg(content: &Content, cfg: &Config, root: &RootConfig) -> Decision {
    let Some(info) = &content.jpeg else {
        return Decision::skip(Reason::UnparsableHeader, 0);
    };
    if info.components == 4 {
        return Decision::skip(Reason::Cmyk, info.quality);
    }
    if info.width == 0 || info.height == 0 {
        return Decision::skip(Reason::UnparsableHeader, info.quality);
    }

    let approximate = !info.quality_confident;
    let lossless_only = root.policy == RootPolicy::LosslessOnly;

    if !lossless_only {
        // A downscale is always worth it: it frees more than any re-encode and
        // keeps 2048px as the best available quality.
        if cfg.jpeg.max_dimension > 0 {
            let longest = info.width.max(info.height);
            if longest > cfg.jpeg.max_dimension {
                return Decision {
                    action: Action::JpegReencode,
                    reason: None,
                    resize_to: Some(cfg.jpeg.max_dimension),
                    stored_quality: info.quality,
                    quality_approximate: approximate,
                };
            }
        }

        // Approximate tables need a margin: a wrong estimate must never cause a
        // re-encode of a file that is actually already lean.
        let threshold = if approximate {
            cfg.jpeg.reencode_min_quality.saturating_add(3)
        } else {
            cfg.jpeg.reencode_min_quality
        };
        if info.quality >= threshold {
            return Decision {
                action: Action::JpegReencode,
                reason: None,
                resize_to: None,
                stored_quality: info.quality,
                quality_approximate: approximate,
            };
        }
    }

    if cfg.jpeg.lossless_pass {
        Decision {
            action: Action::JpegLossless,
            reason: None,
            resize_to: None,
            stored_quality: info.quality,
            quality_approximate: approximate,
        }
    } else if lossless_only {
        Decision::skip(Reason::LosslessDisabled, info.quality)
    } else {
        Decision::skip(Reason::QualityAlreadyLean, info.quality)
    }
}

fn decide_png(content: &Content, cfg: &Config, root: &RootConfig) -> Decision {
    let Some(info) = &content.png else {
        return Decision::skip(Reason::UnparsableHeader, 0);
    };
    if info.width == 0 || info.height == 0 {
        return Decision::skip(Reason::UnparsableHeader, 0);
    }
    // A hand-finished root never gets palette quantisation.
    let lossless_only = root.policy == RootPolicy::LosslessOnly;
    match (cfg.png.mode, lossless_only) {
        (_, true) | (crate::config::PngMode::Lossless, _) => Decision {
            action: Action::PngLossless,
            reason: None,
            resize_to: None,
            stored_quality: 0,
            quality_approximate: false,
        },
        (crate::config::PngMode::Skip, _) => Decision::skip(Reason::FormatDisabled, 0),
        (crate::config::PngMode::QuantIfSafe, false) => {
            if info.has_icc {
                // pngquant drops the profile; losing colour accuracy is never
                // worth the bytes on a file that carries one.
                Decision {
                    action: Action::PngLossless,
                    reason: Some(Reason::PngHasIcc),
                    resize_to: None,
                    stored_quality: 0,
                    quality_approximate: false,
                }
            } else if info.palette_candidate() {
                Decision {
                    action: Action::PngQuant,
                    reason: None,
                    resize_to: None,
                    stored_quality: 0,
                    quality_approximate: false,
                }
            } else {
                Decision {
                    action: Action::PngLossless,
                    reason: Some(Reason::PngHasAlpha),
                    resize_to: None,
                    stored_quality: 0,
                    quality_approximate: false,
                }
            }
        }
    }
}

fn decide_webp(content: &Content, cfg: &Config) -> Decision {
    let Some(info) = &content.webp else {
        return Decision::skip(Reason::UnparsableHeader, 0);
    };
    if info.lossless && cfg.webp.lossless_only {
        Decision {
            action: Action::WebpLossless,
            reason: None,
            resize_to: None,
            stored_quality: 0,
            quality_approximate: false,
        }
    } else {
        Decision::skip(Reason::WebpLossy, 0)
    }
}

/// Parse a duration like `30m`, `5h`, `90s`, `2d` or a plain number of seconds.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let (value, unit) = t.split_at(t.len() - 1);
    let (raw, multiplier) = match unit {
        "s" | "S" => (value, 1u64),
        "m" | "M" => (value, 60),
        "h" | "H" => (value, 3600),
        "d" | "D" => (value, 86_400),
        _ => (t, 1u64),
    };
    let n: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(n.checked_mul(multiplier)?))
}

/// Parse a size like `2GiB`, `500MB`, `1G` or a plain byte count.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let (num, mult) = if let Some(v) = lower.strip_suffix("gib") {
        (v, 1u64 << 30)
    } else if let Some(v) = lower.strip_suffix("mib") {
        (v, 1u64 << 20)
    } else if let Some(v) = lower.strip_suffix("kib") {
        (v, 1u64 << 10)
    } else if let Some(v) = lower.strip_suffix("gb") {
        (v, 1_000_000_000)
    } else if let Some(v) = lower.strip_suffix("mb") {
        (v, 1_000_000)
    } else if let Some(v) = lower.strip_suffix("kb") {
        (v, 1_000)
    } else if let Some(v) = lower.strip_suffix('g') {
        (v, 1u64 << 30)
    } else if let Some(v) = lower.strip_suffix('m') {
        (v, 1u64 << 20)
    } else if let Some(v) = lower.strip_suffix('k') {
        (v, 1u64 << 10)
    } else if let Some(v) = lower.strip_suffix('b') {
        (v, 1)
    } else {
        (lower.as_str(), 1)
    };
    let value: f64 = num.trim().parse().ok()?;
    Some((value * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg::{JpegInfo, Subsampling};
    use crate::png::PngInfo;

    fn jpeg_info(quality: u8, width: u32, height: u32) -> Content {
        Content {
            format: Some(Format::Jpeg),
            jpeg: Some(JpegInfo {
                width,
                height,
                components: 3,
                progressive: true,
                precision: 8,
                subsampling: Subsampling::H2V2,
                quality,
                quality_confident: true,
                orientation: 1,
                has_icc: false,
                icc_bytes: 0,
                has_exif: false,
                has_xmp: false,
                has_iptc: false,
                metadata_bytes: 0,
                comments: Vec::new(),
                header_bytes: 600,
                truncated: false,
            }),
            ..Default::default()
        }
    }

    fn root() -> RootConfig {
        RootConfig {
            path: "/home/example/www".into(),
            owner: None,
            policy: RootPolicy::Optimise,
            keep: vec!["icc".into()],
            enabled: true,
            managed_by_remote: false,
            max_bytes_per_run: None,
            max_files_per_run: None,
            exclude: vec![],
        }
    }

    fn facts(content: &Content) -> Facts<'_> {
        Facts {
            content,
            size: 500_000,
            age: Duration::from_secs(86_400),
        }
    }

    #[test]
    fn high_quality_jpeg_is_reencoded() {
        let cfg = Config::default();
        let c = jpeg_info(95, 1200, 800);
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::JpegReencode);
        assert_eq!(d.resize_to, None);
    }

    #[test]
    fn lean_jpeg_gets_lossless_pass_only() {
        let cfg = Config::default();
        let c = jpeg_info(82, 1200, 800);
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::JpegLossless);
    }

    #[test]
    fn quality_85_is_below_the_reencode_threshold() {
        let cfg = Config::default();
        let c = jpeg_info(85, 1200, 800);
        assert_eq!(
            decide(&facts(&c), &cfg, &root()).action,
            Action::JpegLossless
        );
        let c = jpeg_info(86, 1200, 800);
        assert_eq!(
            decide(&facts(&c), &cfg, &root()).action,
            Action::JpegReencode,
            "our own q85 output must never be re-encoded again"
        );
    }

    #[test]
    fn oversized_jpeg_is_resized_even_when_lean() {
        let cfg = Config::default();
        let c = jpeg_info(70, 6000, 4000);
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::JpegReencode);
        assert_eq!(d.resize_to, Some(2048));
    }

    #[test]
    fn approximate_quality_needs_margin() {
        let cfg = Config::default();
        let mut c = jpeg_info(87, 1200, 800);
        c.jpeg.as_mut().unwrap().quality_confident = false;
        // 87 is above 86 but below 86+3, so it stays lossless.
        assert_eq!(
            decide(&facts(&c), &cfg, &root()).action,
            Action::JpegLossless
        );
        let mut c = jpeg_info(92, 1200, 800);
        c.jpeg.as_mut().unwrap().quality_confident = false;
        assert_eq!(
            decide(&facts(&c), &cfg, &root()).action,
            Action::JpegReencode
        );
    }

    #[test]
    fn cmyk_is_skipped() {
        let cfg = Config::default();
        let mut c = jpeg_info(95, 1200, 800);
        c.jpeg.as_mut().unwrap().components = 4;
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::Skip);
        assert_eq!(d.reason, Some(Reason::Cmyk));
    }

    #[test]
    fn lossless_only_root_never_reencodes() {
        let cfg = Config::default();
        let mut r = root();
        r.policy = RootPolicy::LosslessOnly;
        let c = jpeg_info(99, 6000, 4000);
        let d = decide(&facts(&c), &cfg, &r);
        assert_eq!(d.action, Action::JpegLossless);
        assert_eq!(d.resize_to, None);
    }

    #[test]
    fn opaque_png_gets_palette() {
        let cfg = Config::default();
        let c = Content {
            format: Some(Format::Png),
            png: Some(PngInfo {
                width: 1000,
                height: 800,
                bit_depth: 8,
                colour: Some(crate::png::PngColour::Rgb),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(decide(&facts(&c), &cfg, &root()).action, Action::PngQuant);
    }

    #[test]
    fn png_with_icc_skips_palette() {
        let cfg = Config::default();
        let c = Content {
            format: Some(Format::Png),
            png: Some(PngInfo {
                width: 1000,
                height: 800,
                bit_depth: 8,
                colour: Some(crate::png::PngColour::Rgb),
                has_icc: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::PngLossless);
        assert_eq!(d.reason, Some(Reason::PngHasIcc));
    }

    #[test]
    fn transparent_png_gets_lossless() {
        let cfg = Config::default();
        let c = Content {
            format: Some(Format::Png),
            png: Some(PngInfo {
                width: 1000,
                height: 800,
                bit_depth: 8,
                colour: Some(crate::png::PngColour::Rgba),
                has_alpha: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = decide(&facts(&c), &cfg, &root());
        assert_eq!(d.action, Action::PngLossless);
    }

    #[test]
    fn small_and_recent_files_are_skipped() {
        let cfg = Config::default();
        let c = jpeg_info(99, 1200, 800);
        let mut f = facts(&c);
        f.size = 1024;
        assert_eq!(decide(&f, &cfg, &root()).reason, Some(Reason::TooSmall));
        let mut f = facts(&c);
        f.age = Duration::from_secs(60);
        assert_eq!(decide(&f, &cfg, &root()).reason, Some(Reason::TooNew));
    }

    #[test]
    fn gif_and_avif_are_not_handled() {
        let cfg = Config::default();
        let c = Content {
            format: Some(Format::Gif),
            ..Default::default()
        };
        assert_eq!(
            decide(&facts(&c), &cfg, &root()).reason,
            Some(Reason::FormatNotHandled)
        );
    }

    #[test]
    fn duration_and_size_parsing() {
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("5h"), Some(Duration::from_secs(18_000)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_size("2GiB"), Some(2 * (1 << 30)));
        assert_eq!(parse_size("512MB"), Some(512 * 1_000_000));
        assert_eq!(parse_size("4096"), Some(4096));
    }
}
