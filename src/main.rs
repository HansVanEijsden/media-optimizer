//! Command line entry point.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use mediaopt::config::Config;
use mediaopt::engine::Tools;
use mediaopt::manifest::Manifest;
use mediaopt::report;
use mediaopt::run::{self, RunOptions};
use mediaopt::sched::{self, Schedule};
use mediaopt::{limits, VERSION};

#[derive(Parser)]
#[command(
    name = "mediaopt",
    version = VERSION,
    about = "Idempotent, quality-targeted optimiser for large image trees"
)]
struct Cli {
    /// Configuration file (default: $MEDIAOPT_CONFIG or /etc/mediaopt/config.toml)
    #[arg(long, global = true, env = "MEDIAOPT_CONFIG")]
    config: Option<PathBuf>,

    /// Write logs to this file (daily rotation)
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,

    /// Emit JSON logs on stdout (for container log collection)
    #[arg(long, global = true)]
    log_json: bool,

    /// Override limits.jobs
    #[arg(long, global = true)]
    jobs: Option<usize>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Optimise everything that needs it, and replace files atomically
    Run {
        /// Show what would happen without touching anything
        #[arg(long)]
        dry_run: bool,
        /// Ignore the per-run byte and file budgets
        #[arg(long)]
        no_budget: bool,
    },
    /// Alias for `run --dry-run`
    Plan,
    /// Print lifetime totals from the manifest
    Report {
        /// Also list the most recent runs
        #[arg(long, default_value_t = 10)]
        runs: usize,
    },
    /// Show the engine binaries and the effective configuration
    Tools,
    /// Re-check a sample of manifest entries against the files on disk
    Verify {
        #[arg(long, default_value_t = 200)]
        sample: usize,
    },
    /// Write the manifest as a gzipped JSON snapshot
    Export {
        #[arg(long)]
        out: PathBuf,
    },
    /// Rebuild the manifest from a JSON snapshot
    Import {
        #[arg(long)]
        input: PathBuf,
    },
    /// Reclaim space in the manifest database
    Compact,
    /// Run on a daily schedule forever (the container entry point)
    Schedule,

    /// Remote mode, server side: list the files that still need work as TSV
    Scan {
        /// Limit to one configured root
        #[arg(long)]
        root: Option<PathBuf>,
        /// Stop after this many candidates (0 = all)
        #[arg(long, default_value_t = 0)]
        limit: u64,
        /// Write to this file instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Remote mode, client side: encode one downloaded batch
    EncodeBatch {
        /// Candidate list from `scan`
        #[arg(long)]
        list: PathBuf,
        /// Directory holding the downloaded files, mirroring their absolute paths
        #[arg(long)]
        input: PathBuf,
        /// Directory for the encoded files (index-named)
        #[arg(long)]
        output: PathBuf,
        /// Where to write the result list
        #[arg(long)]
        result: PathBuf,
        /// Worker threads on this machine
        #[arg(long, default_value_t = 8)]
        jobs: usize,
    },

    /// Remote mode, server side: verify and install encoded files
    ApplyStaged {
        /// Directory holding the encoded files
        #[arg(long)]
        input: PathBuf,
        /// Result list from `encode-batch` (`-` for stdin)
        #[arg(long)]
        result: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut cfg = Config::load(&config_path)?;
    // --log-file wins; otherwise fall back to state.log_file so a host install can
    // configure logging in one place.
    let log_file = cli.log_file.clone().or_else(|| cfg.state.log_file.clone());
    let _log_guard = init_logging(&cli, log_file.as_deref())?;
    if let Some(jobs) = cli.jobs {
        cfg.limits.jobs = jobs;
    }

    // Rayon pool size follows the configured job count.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.limits.jobs.max(1))
        .build_global();

    let tools = Tools::discover(&cfg.engine);

    match cli.command {
        Command::Run { dry_run, no_budget } => {
            let mut cfg = cfg;
            if no_budget {
                cfg.limits.max_bytes_per_run = 0;
                cfg.limits.max_files_per_run = 0;
            }
            require_engines(&tools)?;
            let mode = if dry_run { "dry-run" } else { "apply" };
            let report = run::run(&cfg, &tools, RunOptions { dry_run }, mode)?;
            println!("{}", report::human_summary(&report));
            println!("{}", report::summary_line(&report));
            let host = mediaopt::hostname();
            match report::write_run_file(&report, cfg.report_dir(), &host, mode, &tools.describe())
            {
                Ok(path) => tracing::debug!("wrote {}", path.display()),
                Err(e) => tracing::warn!("could not write the run report: {e:#}"),
            }
            let pruned = report::prune(cfg.report_dir(), cfg.state.report_days).unwrap_or(0);
            if pruned > 0 {
                tracing::debug!("pruned {pruned} old report(s)");
            }
            Ok(())
        }
        Command::Plan => {
            let mut cfg = cfg;
            cfg.limits.max_bytes_per_run = 0;
            cfg.limits.max_files_per_run = 0;
            require_engines(&tools)?;
            let report = run::run(&cfg, &tools, RunOptions { dry_run: true }, "dry-run")?;
            println!("{}", report::human_summary(&report));
            Ok(())
        }
        Command::Report { runs } => {
            let manifest = Manifest::open(&cfg.state.db)?;
            print!("{}", report::lifetime_report(&manifest)?);
            if runs > 0 {
                println!();
                for run in manifest.recent_runs(runs as i64)? {
                    print!("{}", report::run_line(&run));
                }
            }
            Ok(())
        }
        Command::Tools => {
            print_tools(&cfg, &config_path, &tools);
            Ok(())
        }
        Command::Verify { sample } => verify(&cfg, sample),
        Command::Export { out } => export(&cfg, &out),
        Command::Import { input } => import(&cfg, &input),
        Command::Compact => compact(&cfg),
        Command::Schedule => schedule(&cfg, &tools),
        Command::Scan { root, limit, out } => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let mut sink: Box<dyn std::io::Write> = match out {
                Some(path) => Box::new(std::io::BufWriter::new(std::fs::File::create(&path)?)),
                None => Box::new(&mut lock),
            };
            let stats = mediaopt::remote::scan(&cfg, root.as_deref(), limit, &mut sink)?;
            sink.flush()?;
            drop(sink);
            eprintln!(
                "scan: {} candidate(s){}",
                stats.candidates,
                if limit > 0 {
                    format!(" (limit {limit})")
                } else {
                    String::new()
                }
            );
            Ok(())
        }
        Command::EncodeBatch {
            list,
            input,
            output,
            result,
            jobs,
        } => {
            require_engines(&tools)?;
            let candidates = mediaopt::remote::read_candidates(&list)?;
            if candidates.is_empty() {
                eprintln!("nothing to encode");
                return Ok(());
            }
            let results =
                mediaopt::remote::encode_batch(&cfg, &tools, &candidates, &input, &output, jobs)?;
            let mut file = std::io::BufWriter::new(std::fs::File::create(&result)?);
            use std::io::Write as _;
            let mut ok = 0;
            for r in &results {
                if r.status == "ok" {
                    ok += 1;
                }
                writeln!(file, "{}", r.to_tsv())?;
            }
            file.flush()?;
            eprintln!(
                "encode-batch: {} file(s), {ok} worth applying, {} rejected",
                results.len(),
                results.len() - ok
            );
            Ok(())
        }
        Command::ApplyStaged { input, result } => {
            require_engines(&tools)?;
            let results = mediaopt::remote::read_results(&result)?;
            let stats = mediaopt::remote::apply_staged(&cfg, &tools, &input, &results)?;
            println!(
                "apply-staged: {} checked, {} replaced, {} rejected, {} errors, {} freed",
                stats.encoded,
                stats.changed,
                stats.rejected,
                stats.errors,
                limits::human_bytes(stats.saved())
            );
            Ok(())
        }
    }
}

fn print_tools(cfg: &Config, config_path: &Path, tools: &Tools) {
    println!("mediaopt {VERSION}");
    println!("config       {}", config_path.display());
    println!("manifest     {}", cfg.state.db.display());
    println!("reports      {}", cfg.state.report_dir.display());
    println!("engines      {}", tools.describe());
    println!(
        "jpeg         mode={:?} floor={} fixed_q={} reencode_min_q={} max_dim={} kernel={}",
        cfg.jpeg.quality_mode,
        cfg.jpeg.ssimu2_floor,
        cfg.jpeg.fixed_quality,
        cfg.jpeg.reencode_min_quality,
        cfg.jpeg.max_dimension,
        cfg.jpeg.resize_kernel
    );
    println!(
        "png          mode={:?} quality_floor={} effort={}",
        cfg.png.mode, cfg.png.quality_floor, cfg.png.lossless_effort
    );
    println!(
        "limits       jobs={} max_runtime={} min_age={} reserve={} bytes/run={} files/run={}",
        cfg.limits.jobs,
        cfg.limits.max_runtime,
        cfg.limits.min_file_age,
        cfg.limits.free_space_reserve,
        cfg.limits.max_bytes_per_run,
        cfg.limits.max_files_per_run
    );
    println!("roots        {}", cfg.active_roots().count());
    for root in cfg.active_roots() {
        println!("  {:<64} policy={:?}", root.path.display(), root.policy);
    }
    for root in cfg.roots.iter().filter(|r| r.managed_by_remote) {
        println!(
            "  {:<64} (remote-managed, skipped here)",
            root.path.display()
        );
    }
    let missing = tools.missing_required();
    if !missing.is_empty() {
        println!(
            "\nWARNING: required engines missing: {}",
            missing.join(", ")
        );
    }
}

fn schedule(cfg: &Config, tools: &Tools) -> Result<()> {
    let spec = std::env::var("MEDIAOPT_SCHEDULE").unwrap_or_else(|_| "04:15".to_string());
    let schedule = Schedule::parse(&spec)?;
    require_engines(tools)?;
    sched::install_signal_handlers();

    // Catch-up: if the last recorded run is overdue, run now instead of waiting.
    let overdue = {
        let manifest = Manifest::open(&cfg.state.db)?;
        let last = manifest
            .last_run()?
            .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.started).ok())
            .map(|d| d.with_timezone(&chrono::Local));
        let now = chrono::Local::now();
        let overdue = schedule.is_overdue(last, now);
        if overdue {
            tracing::info!(
                "last run {} is overdue for schedule {}; running now",
                last.map(|l| l.to_rfc3339())
                    .unwrap_or_else(|| "never".to_string()),
                schedule.to_string()
            );
        }
        overdue
    };

    // Burn-down: keep working until a run ends because the tree is exhausted,
    // instead of stopping at the nightly time limit and waiting a day. Meant for
    // a one-off backlog on a host that can spare the CPU; off by default.
    let burn_down = std::env::var("MEDIAOPT_BURNDOWN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if burn_down {
        tracing::info!("burn-down mode: a run that hits max_runtime starts another");
    }
    let job = || -> Result<()> {
        loop {
            if sched::shutdown_requested() {
                return Ok(());
            }
            let report = run::run(cfg, tools, RunOptions { dry_run: false }, "scheduled")?;
            println!("{}", report::summary_line(&report));
            let _ = report::write_run_file(
                &report,
                cfg.report_dir(),
                &mediaopt::hostname(),
                "scheduled",
                &tools.describe(),
            );
            let _ = report::prune(cfg.report_dir(), cfg.state.report_days);
            let more_work = report.stopped == Some("max_runtime");
            if !(burn_down && more_work) {
                return Ok(());
            }
            tracing::info!("run hit max_runtime; starting another (burn-down)");
            if !sched::sleep_interruptible(Duration::from_secs(30), Duration::from_secs(5)) {
                return Ok(());
            }
        }
    };

    if overdue {
        if let Err(e) = job() {
            tracing::error!("catch-up run failed: {e:#}");
        }
    }
    sched::run_forever(schedule, Duration::from_secs(30), job)
}

fn verify(cfg: &Config, sample: usize) -> Result<()> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let rows = manifest.sample_changed(sample as i64)?;
    if rows.is_empty() {
        println!("nothing to verify yet");
        return Ok(());
    }
    let tools = Tools::discover(&cfg.engine);
    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut wrong_size = 0usize;
    let mut undecodable = 0usize;
    for row in &rows {
        let Ok(meta) = std::fs::metadata(&row.path) else {
            missing += 1;
            println!("MISSING      {}", row.path.display());
            continue;
        };
        if Some(meta.len()) != row.size_after {
            wrong_size += 1;
            println!(
                "SIZE-CHANGED {} (expected {:?}, found {})",
                row.path.display(),
                row.size_after,
                meta.len()
            );
            continue;
        }
        match mediaopt::engine::header_field(&tools, &row.path, "width") {
            Ok(Some(_)) => ok += 1,
            _ => {
                undecodable += 1;
                println!("UNDECODABLE  {}", row.path.display());
            }
        }
    }
    println!(
        "\nchecked {} files: {ok} ok, {missing} missing, {wrong_size} changed size, {undecodable} undecodable",
        rows.len()
    );
    if missing + wrong_size + undecodable > 0 {
        bail!("verification found problems");
    }
    Ok(())
}

fn export(cfg: &Config, out: &Path) -> Result<()> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let json = manifest.export_json()?;
    let compressed = {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(json.as_bytes())?;
        encoder.finish()?
    };
    let path = if out.extension().map(|e| e == "gz").unwrap_or(false) {
        out.to_path_buf()
    } else {
        out.with_extension("json.gz")
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, compressed).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "wrote {} ({} rows)",
        path.display(),
        manifest.count_files()?
    );
    Ok(())
}

fn import(cfg: &Config, input: &Path) -> Result<()> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let raw = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let json = if input.extension().map(|e| e == "gz").unwrap_or(false) {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
        let mut out = String::new();
        decoder.read_to_string(&mut out)?;
        out
    } else {
        String::from_utf8_lossy(&raw).to_string()
    };
    let imported = manifest.import_json(&json)?;
    println!("imported {imported} rows into {}", cfg.state.db.display());
    Ok(())
}

fn compact(cfg: &Config) -> Result<()> {
    let manifest = Manifest::open(&cfg.state.db)?;
    let before = std::fs::metadata(&cfg.state.db)
        .map(|m| m.len())
        .unwrap_or(0);
    manifest.prune_old(180)?;
    manifest.vacuum()?;
    let after = std::fs::metadata(&cfg.state.db)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "compacted manifest {} -> {}",
        limits::human_bytes(before),
        limits::human_bytes(after)
    );
    Ok(())
}

fn require_engines(tools: &Tools) -> Result<()> {
    let missing = tools.missing_required();
    if !missing.is_empty() {
        bail!(
            "required engine binaries are missing: {}. Set them in the [engine] section or via \
             VIPS_BIN / JPEGTRAN_BIN, and check `mediaopt tools`.",
            missing.join(", ")
        );
    }
    Ok(())
}

fn default_config_path() -> PathBuf {
    let candidates = [
        PathBuf::from("/etc/mediaopt/config.toml"),
        PathBuf::from("mediaopt.toml"),
        PathBuf::from("config.toml"),
    ];
    for c in &candidates {
        if Path::new(c).is_file() {
            return c.clone();
        }
    }
    candidates[0].clone()
}

fn init_logging(
    cli: &Cli,
    log_file: Option<&Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;

    // oxipng logs one INFO line per PNG through the `log` bridge; at one line
    // every few seconds it drowns out the run's own progress lines.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,oxipng=error"));
    let stdout_layer = if cli.log_json {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stdout)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .compact()
            .boxed()
    };

    match log_file {
        Some(path) => {
            let dir = path.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating log directory {}", dir.display()))?;
            let prefix = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "mediaopt.log".to_string());
            let appender = tracing_appender::rolling::daily(dir, prefix);
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(false)
                .boxed();
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .init();
            Ok(Some(guard))
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .init();
            Ok(None)
        }
    }
}
