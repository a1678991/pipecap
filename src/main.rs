//! pipecap: inspect Apple Silicon display-pipe allocation and cap a monitor's
//! EDID so that more external displays fit.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod edid;

#[cfg(target_os = "macos")]
mod agent;
#[cfg(target_os = "macos")]
mod iokit;

#[cfg(not(target_os = "macos"))]
compile_error!("pipecap only runs on macOS (Apple Silicon)");

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use edid::{CapOptions, EdidInfo};
use iokit::{CrossbarState, Display, PipeLimits};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Inspect and fix display-pipe allocation on Apple Silicon Macs.
///
/// The display coprocessor reserves display pipes from the highest mode in a
/// monitor's EDID, not from the refresh rate you pick. A 4K monitor that
/// advertises more than ~150 Hz takes two pipes. pipecap shows that
/// allocation and can install a "virtual" EDID that hides the highest modes,
/// which frees a pipe for another monitor.
#[derive(Parser)]
#[command(name = "pipecap", version, about, long_about)]
struct Cli {
    /// Machine readable JSON output where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show pipe allocation, pending displays and a suggestion (default).
    Status,
    /// List external display outputs and their EDIDs.
    List,
    /// Decode an EDID from a file or a connected display.
    Decode {
        /// Path to a binary EDID file.
        file: Option<PathBuf>,
        #[command(flatten)]
        sel: Selector,
    },
    /// Save the EDID of a connected display to a file.
    Dump {
        #[command(flatten)]
        sel: Selector,
        /// Output path (default: <id>-<name>.bin in the current directory).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Remove modes above a refresh rate from a display's EDID and apply it.
    Cap {
        #[command(flatten)]
        sel: Selector,
        /// Highest refresh rate (Hz) to keep. Omit to pick the highest rate
        /// that fits a single pipe for this display.
        #[arg(long)]
        max_hz: Option<f64>,
        /// Also write the capped EDID to this file.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Only show what would change; do not touch the display.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait for the crossbar to re-allocate after applying.
        #[arg(long, default_value_t = 12)]
        wait: u64,
    },
    /// Apply an EDID file to a display as a virtual EDID.
    Apply {
        #[command(flatten)]
        sel: Selector,
        /// Binary EDID file to install.
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value_t = 12)]
        wait: u64,
    },
    /// Remove the virtual EDID from one display (or all with --all).
    Reset {
        #[arg(long, conflicts_with = "all")]
        display: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Keep a cap applied: re-apply whenever the crossbar shows the display
    /// back at its full EDID (after re-plug, wake, or reboot).
    Watch {
        #[command(flatten)]
        sel: Selector,
        #[arg(long)]
        max_hz: Option<f64>,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 10)]
        interval: u64,
    },
    /// Manage a LaunchAgent that runs `pipecap watch` at login.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Install and start the LaunchAgent for a display.
    Install {
        #[command(flatten)]
        sel: Selector,
        #[arg(long)]
        max_hz: Option<f64>,
        #[arg(long, default_value_t = 10)]
        interval: u64,
    },
    /// Stop and remove the LaunchAgent for a display.
    Uninstall {
        #[arg(long)]
        display: String,
    },
    /// Show whether the LaunchAgent for a display is loaded.
    Status {
        #[arg(long)]
        display: String,
    },
}

#[derive(Args, Clone)]
struct Selector {
    /// Display to act on: EDID id (e.g. 10ac1234), a unique part of its
    /// name (e.g. "U2723"), or its index from `pipecap list`.
    #[arg(long)]
    display: Option<String>,
}

fn gpx(v: f64) -> String {
    format!("{:.3} Gpx/s", v / 1e9)
}

fn read_file(p: &Path) -> Result<Vec<u8>> {
    std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))
}

/// Resolve `--display` to one of the connected outputs.
fn resolve<'a>(displays: &'a [Display], sel: &Option<String>) -> Result<&'a Display> {
    let with_edid: Vec<&Display> = displays.iter().filter(|d| d.edid.is_some()).collect();
    let Some(sel) = sel else {
        return match with_edid.as_slice() {
            [d] => Ok(d),
            [] => bail!("no external display with a readable EDID is connected"),
            _ => bail!(
                "several displays are connected; choose one with --display (see `pipecap list`)"
            ),
        };
    };
    let s = sel.trim().to_ascii_lowercase();
    if let Some(d) = with_edid
        .iter()
        .find(|d| d.edid_id().as_deref() == Some(s.as_str()))
    {
        return Ok(d);
    }
    if let Ok(i) = s.parse::<usize>() {
        if let Some(d) = displays.get(i) {
            return Ok(d);
        }
    }
    let by_name: Vec<&Display> = with_edid
        .iter()
        .copied()
        .filter(|d| {
            d.edid
                .as_deref()
                .and_then(|e| edid::parse(e).ok())
                .map(|i| i.label().to_ascii_lowercase().contains(&s))
                .unwrap_or(false)
        })
        .collect();
    match by_name.as_slice() {
        [d] => Ok(d),
        [] => bail!("no connected display matches {sel:?} (see `pipecap list`)"),
        _ => bail!("{sel:?} matches more than one display; use the EDID id instead"),
    }
}

fn info_of(d: &Display) -> Result<EdidInfo> {
    let e = d.edid.as_deref().ok_or_else(|| {
        anyhow!(
            "display {} has no readable EDID ({})",
            d.index,
            iokit::ioreturn_message(d.copy_edid_status)
        )
    })?;
    edid::parse(e).context("EDID parse failed")
}

fn limits() -> (PipeLimits, bool) {
    match iokit::pipe_limits() {
        Some(l) => (l, true),
        None => (PipeLimits::FALLBACK, false),
    }
}

/// Pick the highest common refresh rate that fits one pipe for this display.
fn auto_max_hz(info: &EdidInfo, lim: &PipeLimits) -> Result<f64> {
    let m = info.max_timing().context("EDID has no timings")?;
    let hz = edid::max_hz_for(m.h_active, m.v_active, lim.max_active_pixel_rate as f64);
    // Prefer a rate that is actually advertised, otherwise the raw bound.
    let advertised = info
        .timings
        .iter()
        .filter(|t| t.h_active == m.h_active && t.v_active == m.v_active)
        .map(|t| t.refresh_hz.round() as u32)
        .filter(|&r| r <= hz)
        .max();
    Ok(advertised.unwrap_or(hz) as f64)
}

#[derive(Serialize)]
struct StatusReport {
    pipe_count: usize,
    limits: PipeLimits,
    limits_from_registry: bool,
    crossbar: Option<CrossbarState>,
    active: Vec<iokit::Framebuffer>,
    displays: Vec<DisplaySummary>,
    suggestions: Vec<String>,
}

#[derive(Serialize)]
struct DisplaySummary {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    pipe: Option<String>,
    edid_bytes: usize,
    max_mode: Option<String>,
    max_active_pixel_rate: Option<f64>,
    copy_edid_status: i32,
}

fn summarize(d: &Display) -> DisplaySummary {
    let info = d.edid.as_deref().and_then(|e| edid::parse(e).ok());
    let max = info.as_ref().and_then(|i| i.max_timing().cloned());
    DisplaySummary {
        index: d.index,
        id: d.edid_id(),
        name: info.as_ref().map(|i| i.label()),
        pipe: d.pipe.clone(),
        edid_bytes: d.edid.as_ref().map(Vec::len).unwrap_or(0),
        max_mode: max
            .as_ref()
            .map(|m| format!("{}x{}@{:.0}", m.h_active, m.v_active, m.refresh_hz)),
        max_active_pixel_rate: max.as_ref().map(|m| m.active_pixel_rate()),
        copy_edid_status: d.copy_edid_status,
    }
}

fn build_status() -> Result<StatusReport> {
    let (lim, from_reg) = limits();
    let crossbar = iokit::crossbar()?;
    let displays = iokit::external_displays().unwrap_or_default();
    let pipe_count = iokit::pipe_count();
    let mut suggestions = Vec::new();
    if let Some(x) = &crossbar {
        let used: i64 = x.mappings.iter().map(|m| m.pipe_ids.len() as i64).sum();
        let needed: i64 = x.mappings.iter().map(|m| m.max_pipes).sum();
        if !x.pending.is_empty() || (pipe_count > 0 && needed > pipe_count as i64) {
            for m in x.mappings.iter().filter(|m| m.max_pipes > 1) {
                let hz = edid::max_hz_for(
                    m.max_w as u32,
                    m.max_h as u32,
                    lim.max_active_pixel_rate as f64,
                );
                let pick = if hz >= 144 { 144 } else { hz };
                suggestions.push(format!(
                    "cap \"{}\" to <= {} Hz ({}x{} fits one pipe up to {} Hz): pipecap cap --display \"{}\" --max-hz {}",
                    m.product_name, pick, m.max_w, m.max_h, hz, m.product_name, pick
                ));
            }
            if suggestions.is_empty() {
                suggestions.push(format!(
                    "{used} of {pipe_count} pipes are in use and nothing needs more than one pipe; \
                     the pending display may be limited by port or dock bandwidth instead"
                ));
            }
        }
    }
    Ok(StatusReport {
        pipe_count,
        limits: lim,
        limits_from_registry: from_reg,
        crossbar,
        active: iokit::active_framebuffers(),
        displays: displays.iter().map(summarize).collect(),
        suggestions,
    })
}

fn print_status(r: &StatusReport) {
    println!(
        "External display pipes: {}   single-pipe limit: {} active / {} total{}",
        r.pipe_count,
        gpx(r.limits.max_active_pixel_rate as f64),
        gpx(r.limits.max_total_pixel_rate as f64),
        if r.limits_from_registry {
            ""
        } else {
            " (fallback values)"
        }
    );
    match &r.crossbar {
        Some(x) => {
            println!("Pipe allocation (display crossbar):");
            println!(
                "  {:<24} {:<8} {:<7} {:<10} {:<16} state",
                "display", "addr", "needs", "pipes", "max active"
            );
            for m in &x.mappings {
                let state = match r.active.iter().find(|f| f.product_name == m.product_name) {
                    Some(f) => format!("active, up to {} Hz ({})", f.max_refresh_hz, f.pipe),
                    None if m.pipe_ids.is_empty() => "NO PIPE".to_string(),
                    None => "assigned".to_string(),
                };
                println!(
                    "  {:<24} {:<8} {:<7} {:<10} {:<16} {}",
                    m.product_name,
                    m.address,
                    m.max_pipes,
                    format!("{:?}", m.pipe_ids),
                    gpx(m.max_active_pixel_rate as f64),
                    state
                );
            }
            if x.pending.is_empty() {
                println!("Pending (waiting for a pipe): none");
            } else {
                println!("Pending (waiting for a pipe): {}", x.pending.join(", "));
            }
        }
        None => println!("Display crossbar not found (not an Apple Silicon Mac?)"),
    }
    println!("External outputs:");
    for d in &r.displays {
        println!(
            "  [{}] id={} {:<24} pipe={:<9} EDID {} B  max {}{}",
            d.index,
            d.id.as_deref().unwrap_or("--------"),
            d.name.as_deref().unwrap_or("(no EDID)"),
            d.pipe.as_deref().unwrap_or("?"),
            d.edid_bytes,
            d.max_mode.as_deref().unwrap_or("-"),
            d.max_active_pixel_rate
                .map(|r| format!(" ({})", gpx(r)))
                .unwrap_or_default()
        );
    }
    for s in &r.suggestions {
        println!("Suggestion: {s}");
    }
}

/// Poll the crossbar until the mapping for `info` shows `<= want_pipes`, or timeout.
fn wait_for(info: &EdidInfo, want_pipes: i64, timeout: Duration) -> Result<()> {
    if timeout.is_zero() {
        return Ok(());
    }
    let start = Instant::now();
    eprint!("waiting for the crossbar to re-allocate");
    let mut ok = false;
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(750));
        eprint!(".");
        if let Ok(Some(x)) = iokit::crossbar() {
            if let Some(m) = x.mappings.iter().find(|m| m.matches(info)) {
                if m.max_pipes <= want_pipes && !m.pipe_ids.is_empty() && x.pending.is_empty() {
                    ok = true;
                    break;
                }
            }
        }
    }
    eprintln!();
    if !ok {
        eprintln!("note: the crossbar did not settle within the wait time; check `pipecap status`");
    }
    Ok(())
}

fn backup_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    let d = PathBuf::from(home).join("Library/Application Support/pipecap");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Keep a copy of the original EDID before the first override (never overwritten).
fn backup_original(info: &EdidInfo, bytes: &[u8]) -> Result<Option<PathBuf>> {
    let p = backup_dir()?.join(format!(
        "{}-{}-original.bin",
        info.id,
        safe_name(&info.label())
    ));
    if p.exists() {
        return Ok(None);
    }
    std::fs::write(&p, bytes)?;
    Ok(Some(p))
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn cmd_cap(
    sel: &Selector,
    max_hz: Option<f64>,
    save: Option<PathBuf>,
    dry_run: bool,
    wait: u64,
    json: bool,
) -> Result<()> {
    let displays = iokit::external_displays()?;
    let d = resolve(&displays, &sel.display)?;
    let info = info_of(d)?;
    let bytes = d.edid.as_deref().expect("resolved display has EDID");
    let (lim, _) = limits();
    let max_hz = match max_hz {
        Some(h) => h,
        None => {
            let h = auto_max_hz(&info, &lim)?;
            eprintln!("auto: {} fits one pipe up to {} Hz", info.label(), h);
            h
        }
    };
    let r = edid::cap(
        bytes,
        &CapOptions {
            max_hz,
            max_pixel_clock_mhz: None,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{} (id {}): cap at {} Hz", info.label(), info.id, max_hz);
        for t in &r.dropped {
            println!("  drop  {t}");
        }
        for n in &r.notes {
            println!("  note  {n}");
        }
        if let Some(m) = &r.max_timing {
            println!(
                "  max after cap: {}x{} @ {:.2} Hz = {} active ({} per pipe)",
                m.h_active,
                m.v_active,
                m.refresh_hz,
                gpx(m.active_pixel_rate()),
                gpx(lim.max_active_pixel_rate as f64)
            );
        }
    }
    if let Some(p) = &save {
        std::fs::write(p, &r.bytes)?;
        eprintln!("saved capped EDID to {}", p.display());
    }
    if r.unchanged {
        eprintln!("nothing to do: the EDID already has no mode above {max_hz} Hz");
        return Ok(());
    }
    if dry_run {
        eprintln!("dry run: not applied");
        return Ok(());
    }
    if let Some(p) = backup_original(&info, bytes)? {
        eprintln!("original EDID saved to {}", p.display());
    }
    d.set_virtual_edid(Some(&r.bytes))?;
    eprintln!(
        "virtual EDID applied to {} (the display may blink)",
        info.label()
    );
    wait_for(&info, 1, Duration::from_secs(wait))?;
    if !json {
        print_status(&build_status()?);
    }
    Ok(())
}

fn cmd_watch(sel: &Selector, max_hz: Option<f64>, interval: u64) -> Result<()> {
    let interval = Duration::from_secs(interval.max(1));
    let mut cached: Option<(String, Vec<u8>, f64)> = None; // (id, capped bytes, max active rate)
    let (lim, _) = limits();
    eprintln!("pipecap watch: polling every {}s", interval.as_secs());
    loop {
        let outcome: Result<()> = (|| {
            let displays = iokit::external_displays()?;
            let Ok(d) = resolve(&displays, &sel.display) else {
                return Ok(()); // display not connected right now
            };
            let info = info_of(d)?;
            let bytes = d.edid.as_deref().expect("has EDID");
            let hz = match max_hz {
                Some(h) => h,
                None => auto_max_hz(&info, &lim)?,
            };
            let entry = match &cached {
                Some(c) if c.0 == info.id => c.clone(),
                _ => {
                    let r = edid::cap(
                        bytes,
                        &CapOptions {
                            max_hz: hz,
                            max_pixel_clock_mhz: None,
                        },
                    )?;
                    let rate = r
                        .max_timing
                        .as_ref()
                        .map(|t| t.active_pixel_rate())
                        .unwrap_or(0.0);
                    let c = (info.id.clone(), r.bytes, rate);
                    cached = Some(c.clone());
                    c
                }
            };
            let Some(x) = iokit::crossbar()? else {
                return Ok(());
            };
            let Some(m) = x.mappings.iter().find(|m| m.matches(&info)) else {
                return Ok(());
            };
            if (m.max_active_pixel_rate as f64) > entry.2 * 1.001 {
                eprintln!(
                    "{}: crossbar sees {} (> {}), re-applying cap",
                    info.label(),
                    gpx(m.max_active_pixel_rate as f64),
                    gpx(entry.2)
                );
                d.set_virtual_edid(Some(&entry.1))?;
            }
            Ok(())
        })();
        if let Err(e) = outcome {
            eprintln!("watch: {e:#}");
        }
        std::thread::sleep(interval);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => {
            let r = build_status()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_status(&r);
            }
        }
        Cmd::List => {
            let displays = iokit::external_displays()?;
            if cli.json {
                let v: Vec<DisplaySummary> = displays.iter().map(summarize).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for d in &displays {
                    let s = summarize(d);
                    println!(
                        "[{}] id={} {:<24} pipe={:<9} EDID {} B  max {}  {}",
                        s.index,
                        s.id.as_deref().unwrap_or("--------"),
                        s.name.as_deref().unwrap_or("(no EDID)"),
                        s.pipe.as_deref().unwrap_or("?"),
                        s.edid_bytes,
                        s.max_mode.as_deref().unwrap_or("-"),
                        d.path
                    );
                    if d.edid.is_none() {
                        println!(
                            "    EDID not readable: {}",
                            iokit::ioreturn_message(d.copy_edid_status)
                        );
                    }
                }
            }
        }
        Cmd::Decode { file, sel } => {
            let bytes = match file {
                Some(p) => read_file(&p)?,
                None => {
                    let displays = iokit::external_displays()?;
                    resolve(&displays, &sel.display)?
                        .edid
                        .clone()
                        .context("no EDID")?
                }
            };
            let info = edid::parse(&bytes)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print!("{}", edid::describe(&info));
                match edid::validate(&bytes) {
                    Ok(()) => println!("checksums: ok"),
                    Err(e) => println!("checksums: {e}"),
                }
            }
        }
        Cmd::Dump { sel, output } => {
            let displays = iokit::external_displays()?;
            let d = resolve(&displays, &sel.display)?;
            let info = info_of(d)?;
            let p = output.unwrap_or_else(|| {
                PathBuf::from(format!("{}-{}.bin", info.id, safe_name(&info.label())))
            });
            std::fs::write(&p, d.edid.as_deref().expect("edid"))?;
            println!(
                "wrote {} ({} bytes)",
                p.display(),
                d.edid.as_ref().map(Vec::len).unwrap_or(0)
            );
        }
        Cmd::Cap {
            sel,
            max_hz,
            save,
            dry_run,
            wait,
        } => cmd_cap(&sel, max_hz, save, dry_run, wait, cli.json)?,
        Cmd::Apply { sel, file, wait } => {
            let bytes = read_file(&file)?;
            edid::validate(&bytes).context("refusing to apply an invalid EDID")?;
            let displays = iokit::external_displays()?;
            let d = resolve(&displays, &sel.display)?;
            let info = info_of(d)?;
            let new_info = edid::parse(&bytes)?;
            if new_info.id != info.id {
                bail!(
                    "EDID id mismatch: file is for {} but the display is {}",
                    new_info.id,
                    info.id
                );
            }
            if let Some(p) = backup_original(&info, d.edid.as_deref().expect("edid"))? {
                eprintln!("original EDID saved to {}", p.display());
            }
            d.set_virtual_edid(Some(&bytes))?;
            eprintln!("virtual EDID applied to {}", info.label());
            wait_for(&info, i64::MAX, Duration::from_secs(wait))?;
            print_status(&build_status()?);
        }
        Cmd::Reset { display, all } => {
            let displays = iokit::external_displays()?;
            let targets: Vec<&Display> = if all {
                displays.iter().collect()
            } else {
                vec![resolve(&displays, &display)?]
            };
            for d in targets {
                let label = info_of(d)
                    .map(|i| i.label())
                    .unwrap_or_else(|_| format!("output {}", d.index));
                match d.set_virtual_edid(None) {
                    Ok(()) => println!("reset {label}"),
                    Err(e) => println!("reset {label}: {e:#}"),
                }
            }
        }
        Cmd::Watch {
            sel,
            max_hz,
            interval,
        } => cmd_watch(&sel, max_hz, interval)?,
        Cmd::Agent { cmd } => match cmd {
            AgentCmd::Install {
                sel,
                max_hz,
                interval,
            } => {
                let displays = iokit::external_displays()?;
                let d = resolve(&displays, &sel.display)?;
                let info = info_of(d)?;
                let (lim, _) = limits();
                let hz = match max_hz {
                    Some(h) => h,
                    None => auto_max_hz(&info, &lim)?,
                };
                let p = agent::install(&info.id, hz, interval)?;
                println!(
                    "installed {} for {} (cap {} Hz)",
                    p.display(),
                    info.label(),
                    hz
                );
                println!("log: {}", agent::log_path()?.display());
            }
            AgentCmd::Uninstall { display } => {
                let p = agent::uninstall(&display)?;
                println!("removed {}", p.display());
            }
            AgentCmd::Status { display } => {
                let (p, loaded) = agent::status(&display)?;
                println!(
                    "{}: {}",
                    p.display(),
                    if loaded { "loaded" } else { "not loaded" }
                );
            }
        },
    }
    Ok(())
}
