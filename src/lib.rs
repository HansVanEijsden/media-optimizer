//! mediaopt - an idempotent, quality-targeted optimiser for large image trees.
//!
//! The design and the measurements behind it live in `docs/`.

pub mod config;
pub mod decide;
pub mod engine;
pub mod jpeg;
pub mod limits;
pub mod manifest;
pub mod png;
pub mod probe;
pub mod remote;
pub mod replace;
pub mod report;
pub mod run;
pub mod sched;

/// Semantic version of this build, written into every JPEG COM marker.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Host name, recorded in reports so a shared NAS of reports stays readable.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname into a buffer we own.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).to_string()
    } else {
        "unknown".to_string()
    }
}
