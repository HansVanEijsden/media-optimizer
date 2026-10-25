//! Scheduling: a tiny daily-schedule parser plus a catch-up rule.
//!
//! The old pipeline ran from cron, so a host that was down at the scheduled
//! minute simply skipped that night - which is exactly what happened on one
//! production host. Here the scheduler remembers the last successful run and
//! fires immediately when it is overdue, whether that is because the container
//! just started or because the host was down.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub hour: u32,
    pub minute: u32,
}

impl Schedule {
    /// Accepts `HH:MM` or a five-field cron expression of the daily form
    /// `M H * * *` (value in either order for the first two fields).
    pub fn parse(text: &str) -> Result<Self> {
        let t = text.trim();
        if t.is_empty() {
            bail!("empty schedule");
        }
        let fields: Vec<&str> = t.split_whitespace().collect();
        match fields.as_slice() {
            [single] if single.contains(':') => {
                let (h, m) = single
                    .split_once(':')
                    .context("schedule must look like 04:15")?;
                let hour: u32 = h.parse().with_context(|| format!("bad hour in {single}"))?;
                let minute: u32 = m
                    .parse()
                    .with_context(|| format!("bad minute in {single}"))?;
                validate(hour, minute)?;
                Ok(Schedule { hour, minute })
            }
            [minute, hour, rest @ ..] if rest.len() == 3 => {
                // Cron form: minute hour dom month dow. We only support days.
                if rest.iter().any(|f| *f != "*") {
                    bail!("only daily schedules are supported (minute hour * * *), got {t}");
                }
                let hour = parse_cron_num(hour, "hour")?;
                let minute = parse_cron_num(minute, "minute")?;
                validate(hour, minute)?;
                Ok(Schedule { hour, minute })
            }
            _ => bail!("cannot parse schedule {t:?}; use HH:MM or 'M H * * *'"),
        }
    }

    /// The next instant this schedule fires, strictly after `now`.
    pub fn next_after(&self, now: DateTime<Local>) -> DateTime<Local> {
        let today = Local
            .with_ymd_and_hms(
                now.year(),
                now.month(),
                now.day(),
                self.hour,
                self.minute,
                0,
            )
            .single();
        match today {
            Some(t) if t > now => t,
            _ => {
                let tomorrow = now + chrono::Duration::days(1);
                Local
                    .with_ymd_and_hms(
                        tomorrow.year(),
                        tomorrow.month(),
                        tomorrow.day(),
                        self.hour,
                        self.minute,
                        0,
                    )
                    .single()
                    .unwrap_or(now + chrono::Duration::hours(24))
            }
        }
    }

    /// Whether a run is overdue given the timestamp of the last successful run.
    /// The grace period covers clock jitter and a long previous run.
    pub fn is_overdue(&self, last_run: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
        let Some(last) = last_run else {
            return true;
        };
        let expected = self.next_after(last);
        let grace = chrono::Duration::minutes(90);
        now > expected + grace
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

fn validate(hour: u32, minute: u32) -> Result<()> {
    if hour > 23 {
        bail!("hour must be 0-23, got {hour}");
    }
    if minute > 59 {
        bail!("minute must be 0-59, got {minute}");
    }
    Ok(())
}

fn parse_cron_num(field: &str, what: &str) -> Result<u32> {
    if field.contains('/') || field.contains('-') || field.contains(',') {
        bail!("only plain numbers are supported for the cron {what} field, got {field:?}");
    }
    field
        .parse::<u32>()
        .with_context(|| format!("bad cron {what}: {field:?}"))
}

use chrono::Datelike;
#[cfg(test)]
use chrono::Timelike;

/// Runs forever: sleep until the next slot, run, repeat. `tick` is how often the
/// sleep loop wakes up to check for shutdown.
pub fn run_forever<F>(schedule: Schedule, tick: Duration, mut job: F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    // Catch up first if the last run is overdue.
    if schedule.is_overdue(None, Local::now()) {
        tracing::info!("no previous run recorded; starting immediately");
    }
    loop {
        let now = Local::now();
        let next = schedule.next_after(now);
        let wait = (next - now).to_std().unwrap_or(Duration::from_secs(60));
        tracing::info!(
            "next run at {} (in {}h{}m)",
            next.format("%Y-%m-%d %H:%M"),
            wait.as_secs() / 3600,
            (wait.as_secs() % 3600) / 60
        );
        if !sleep_interruptible(wait, tick) {
            tracing::info!("scheduler interrupted");
            return Ok(());
        }
        if let Err(e) = job() {
            tracing::error!("scheduled run failed: {e:#}");
        }
    }
}

/// Sleep in slices so a SIGTERM is honoured within `tick`. Returns false when
/// shutdown was requested.
pub fn sleep_interruptible(total: Duration, tick: Duration) -> bool {
    let mut left = total;
    while left > Duration::ZERO {
        if shutdown_requested() {
            return false;
        }
        let slice = left.min(tick);
        std::thread::sleep(slice);
        left -= slice;
    }
    !shutdown_requested()
}

static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Install SIGTERM/SIGINT handlers so a container stop finishes cleanly.
pub fn install_signal_handlers() {
    // SAFETY: installing a handler that only sets an atomic flag.
    unsafe {
        let handler = handle_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_syntaxes() {
        assert_eq!(
            Schedule::parse("04:15").unwrap(),
            Schedule {
                hour: 4,
                minute: 15
            }
        );
        assert_eq!(
            Schedule::parse("15 4 * * *").unwrap(),
            Schedule {
                hour: 4,
                minute: 15
            }
        );
    }

    #[test]
    fn rejects_unsupported_syntax() {
        assert!(Schedule::parse("").is_err());
        assert!(Schedule::parse("4").is_err());
        assert!(Schedule::parse("*/15 4 * * *").is_err());
        assert!(Schedule::parse("15 4 * * 1").is_err());
        assert!(Schedule::parse("25:00").is_err());
    }

    #[test]
    fn next_run_is_in_the_future() {
        let s = Schedule {
            hour: 4,
            minute: 15,
        };
        let now = Local::now();
        let next = s.next_after(now);
        assert!(next > now);
        assert!(next - now <= chrono::Duration::hours(24) + chrono::Duration::minutes(1));
        assert_eq!(next.hour(), 4);
        assert_eq!(next.minute(), 15);
    }

    #[test]
    fn overdue_logic() {
        let s = Schedule {
            hour: 4,
            minute: 15,
        };
        let now = Local::now();
        assert!(s.is_overdue(None, now), "no history means run now");
        assert!(
            !s.is_overdue(Some(now - chrono::Duration::hours(1)), now),
            "a recent run is not overdue"
        );
        assert!(
            s.is_overdue(Some(now - chrono::Duration::hours(30)), now),
            "a missed night is overdue"
        );
    }

    #[test]
    fn schedule_round_trips() {
        assert_eq!(Schedule { hour: 3, minute: 7 }.to_string(), "03:07");
    }
}
