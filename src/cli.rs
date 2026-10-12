//! The body of the `poop` command-line client.
//!
//! Four subcommands, in the order you would use them: `scan` to see what is in
//! range, `info` to connect and read the strap's own view of itself, `stream`
//! to watch live traffic, and `history` to offload the ring buffer to a file.
//!
//! `history` is where the interesting rule lives. It writes each chunk to
//! NDJSON and calls `fsync` **before** acknowledging it, because an
//! acknowledged chunk is one the strap may discard. A write error withholds the
//! acknowledgement and ends the session, which leaves the data on the strap for
//! the next attempt rather than losing it to a tidy-looking exit.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::Receiver;

use std::sync::Arc;

use poop::history::{HistorySession, Step, IDLE_TIMEOUT};
use poop::protocol::{
    payload::HapticPattern, DeviceFamily, RebootProbe, SendPolicy, DEVICE_KEYS, FEATURE_FLAGS,
};
use poop::sim::{self, SimConfig};
use poop::store::NdjsonSink;
use poop::strap::Strap;
use poop::types::WhoopEvent;
use poop::whoop_client::{short_id, WhoopClient, WhoopClientConfig, WhoopDevice, WhoopHandle};

// ── Arguments ─────────────────────────────────────────────────────────────────

struct Args {
    command: Command,
    family: Option<DeviceFamily>,
    name_prefix: Option<String>,
    scan_secs: u64,
    /// Connect to the strap whose id or name contains this.
    select: Option<String>,
    /// Where `history` writes, and where `stream --capture` writes.
    out: PathBuf,
    /// Ask for the proprietary live HR stream as well as the standard one.
    realtime: bool,
    /// Stop `stream` after this many seconds.
    duration: Option<u64>,
    /// Set the strap's clock from this host's clock before offloading.
    set_clock: bool,
    /// Drive a simulated strap instead of a real one.
    simulate: bool,
    /// Permit configuration writes. Nothing writes without this.
    allow_writes: bool,
    /// Permit the MG ECG command family.
    allow_ecg: bool,
    /// Positional argument for the subcommands that take one.
    arg: Option<String>,
    /// Second positional argument — a key, an ID, an epoch.
    value: Option<String>,
    /// Third positional argument, which only `config set KEY VALUE` uses.
    extra: Option<String>,
}

#[derive(PartialEq)]
enum Command {
    Scan,
    Info,
    Stream,
    History,
    Buzz,
    Config,
    Alarm,
    Probe,
}

const USAGE: &str = "\
poop — a WHOOP 4.0 / 5.0 / MG client over Bluetooth Low Energy

USAGE:
    poop <COMMAND> [OPTIONS]

COMMANDS:
    scan                 List WHOOP straps in range and stop
    info                 Connect and report identity, battery, clock and range
    stream               Print live traffic until interrupted
    history              Offload the strap's stored records to an NDJSON file
    buzz                 Buzz the strap, to confirm you have the right one
    config <SUB>         Named configuration — see CONFIG below
    alarm <SUB>          Alarms — see ALARM below
    probe <SUB>          Read-only diagnostics — see PROBE below

CONFIG:
    config keys          Walk the device-configuration key names
    config flags         Walk the feature-flag names
    config get <KEY>     Read one named value (device key or feature flag)
    config set <KEY> <V> Write one named value      [needs --allow-writes]
    config disable-r22   Clear every R22 flag, verifying each readback
                                                    [needs --allow-writes]

ALARM:
    alarm get [ID]       Read a stored alarm        (WHOOP 5/MG)
    alarm set <EPOCH_MS> Arm an alarm for an absolute instant
    alarm run [ID]       Fire a stored alarm now    (clears the slot)
    alarm off [ID]       Disarm one slot, or all of them when ID is omitted
    alarm stop           Stop an in-progress haptic

PROBE:
    probe location       Body-location status
    probe battery-pack   Cached battery-pack record (WHOOP 5/MG)
    probe battery-ext    Cached extended battery    (WHOOP 4)
    probe name           Advertised name            (WHOOP 5/MG)
    probe serial         Strap serial from two hellos (WHOOP 4)
    probe reboot <N>     Send restart candidate 1-5 (WHOOP 4, restarts it)

OPTIONS:
    -f, --family <4|5>       Only consider this generation
    -n, --name <PREFIX>      Only consider straps whose name starts with PREFIX
    -s, --scan-secs <N>      Scan window in seconds (default 10)
        --select <TEXT>      Connect to the strap whose name or id contains TEXT
    -o, --out <PATH>         Output file (default whoop-history.ndjson)
        --realtime           Also request the proprietary live HR packet
        --duration <N>       Stop streaming after N seconds
        --set-clock          Set the strap's clock from this host before offload
        --simulate           Drive a simulated strap; no hardware needed
        --allow-writes       Permit configuration writes
        --allow-ecg          Permit the MG ECG command family
    -h, --help               Show this help

EXAMPLES:
    poop scan
    poop info --family 5
    poop stream --simulate --duration 30
    poop history --simulate --out demo.ndjson
    poop config flags
    poop config get cont_collection_mode
    poop alarm set 1760230800000

NOTES:
    Destructive commands are not reachable from this CLI at all. Configuration
    writes and ECG commands need their flags above, and `probe reboot` restarts
    the strap, interrupting anything in flight.

    --simulate runs the real decoders over generated frames. Nothing it reports
    is a measurement.

    RUST_LOG=debug gives BLE-level diagnostics on stderr.
";

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut parsed = Args {
        command: Command::Scan,
        family: None,
        name_prefix: None,
        scan_secs: 10,
        select: None,
        out: PathBuf::from("whoop-history.ndjson"),
        realtime: false,
        duration: None,
        set_clock: false,
        simulate: false,
        allow_writes: false,
        allow_ecg: false,
        arg: None,
        value: None,
        extra: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "scan" => command = Some(Command::Scan),
            "info" => command = Some(Command::Info),
            "stream" => command = Some(Command::Stream),
            "history" => command = Some(Command::History),
            "buzz" => command = Some(Command::Buzz),
            "config" => command = Some(Command::Config),
            "alarm" => command = Some(Command::Alarm),
            "probe" => command = Some(Command::Probe),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-f" | "--family" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--family needs 4 or 5"))?;
                parsed.family = Some(match value.as_str() {
                    "4" | "4.0" => DeviceFamily::Whoop4,
                    "5" | "5.0" | "mg" | "MG" => DeviceFamily::Whoop5,
                    other => return Err(anyhow!("unknown family {other}; use 4 or 5")),
                });
            }
            "-n" | "--name" => {
                parsed.name_prefix = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--name needs a prefix"))?,
                )
            }
            "-s" | "--scan-secs" => {
                parsed.scan_secs = args
                    .next()
                    .ok_or_else(|| anyhow!("--scan-secs needs a number"))?
                    .parse()?
            }
            "--select" => {
                parsed.select = Some(args.next().ok_or_else(|| anyhow!("--select needs text"))?)
            }
            "-o" | "--out" => {
                parsed.out =
                    PathBuf::from(args.next().ok_or_else(|| anyhow!("--out needs a path"))?)
            }
            "--realtime" => parsed.realtime = true,
            "--duration" => {
                parsed.duration = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--duration needs seconds"))?
                        .parse()?,
                )
            }
            "--set-clock" => parsed.set_clock = true,
            "--simulate" => parsed.simulate = true,
            "--allow-writes" => parsed.allow_writes = true,
            "--allow-ecg" => parsed.allow_ecg = true,
            // Positionals, which only the subcommands use. Collected in order
            // rather than by name, because `config set KEY VALUE` is the one
            // shape that needs two and naming them would be worse to type.
            other if !other.starts_with('-') && command.is_some() => {
                if parsed.arg.is_none() {
                    parsed.arg = Some(other.to_owned());
                } else if parsed.value.is_none() {
                    parsed.value = Some(other.to_owned());
                } else if parsed.extra.is_none() {
                    parsed.extra = Some(other.to_owned());
                } else {
                    return Err(anyhow!("unexpected argument {other}"));
                }
            }
            other => return Err(anyhow!("unknown argument {other}; try --help")),
        }
    }

    parsed.command = command.ok_or_else(|| {
        anyhow!("no command given; try one of scan, info, stream, history, buzz, or --help")
    })?;
    Ok(parsed)
}

fn config(args: &Args) -> WhoopClientConfig {
    WhoopClientConfig {
        family: args.family,
        name_prefix: args.name_prefix.clone(),
        scan_timeout_secs: args.scan_secs,
        realtime_hr: args.realtime,
        policy: {
            let mut policy = SendPolicy::READ_ONLY;
            if args.allow_writes {
                policy = policy.with_config_write();
            }
            if args.allow_ecg {
                policy = policy.with_ecg();
            }
            policy
        },
        ..WhoopClientConfig::default()
    }
}

// ── Connecting ────────────────────────────────────────────────────────────────

/// Scan and pick one strap.
///
/// `--select` matches on name or id, because which of the two a user has to
/// hand depends on the platform: macOS never exposes a hardware address, so the
/// id there is a per-host UUID nobody memorises.
async fn pick(args: &Args) -> Result<WhoopDevice> {
    let client = WhoopClient::new(config(args));
    let found = client.scan_all().await?;
    if found.is_empty() {
        return Err(anyhow!(
            "no WHOOP strap found in {} s — is it on your wrist and awake?",
            args.scan_secs
        ));
    }
    match &args.select {
        Some(needle) => {
            let lower = needle.to_lowercase();
            found
                .into_iter()
                .find(|d| {
                    d.name.to_lowercase().contains(&lower) || d.id.to_lowercase().contains(&lower)
                })
                .ok_or_else(|| anyhow!("no strap matching {needle:?}"))
        }
        None => Ok(found.into_iter().next().expect("checked non-empty")),
    }
}

/// Open a strap: a real one, or a simulated one under `--simulate`.
///
/// Returns the event stream, a [`Strap`] covering both cases, and a label for
/// the header line.
async fn open(args: &Args) -> Result<(Receiver<WhoopEvent>, Strap, String)> {
    if args.simulate {
        let config = SimConfig {
            family: args.family.unwrap_or(DeviceFamily::Whoop5),
            ..SimConfig::default()
        };
        let label = format!("{} (simulated)", config.name);
        println!("simulating a {}", config.family.label());
        let (events, handle) = sim::connect(config);
        return Ok((events, Strap::sim(handle), label));
    }

    let device = pick(args).await?;
    println!(
        "connecting to {} ({}) as {}",
        device.name,
        short_id(&device.id),
        device.family.label()
    );
    let client = WhoopClient::new(config(args));
    let (events, handle) = client.connect_to(device.clone()).await?;
    Ok((events, Strap::live(handle), device.name))
}

/// Open a **live** connection, for the commands a simulation cannot answer.
///
/// Configuration, alarms and the probes all read or change strap state that no
/// simulation has. Returning an error beats inventing a feature-flag namespace
/// or an alarm slot, which would make `--simulate` a source of confident
/// nonsense rather than a way to exercise the decoders.
async fn open_live(args: &Args) -> Result<(Receiver<WhoopEvent>, Arc<WhoopHandle>, WhoopDevice)> {
    if args.simulate {
        return Err(anyhow!(
            "this command needs a real strap: there is no simulated configuration, \
             alarm or diagnostic state to read"
        ));
    }
    let device = pick(args).await?;
    println!(
        "connecting to {} ({}) as {}",
        device.name,
        short_id(&device.id),
        device.family.label()
    );
    let client = WhoopClient::new(config(args));
    let (events, handle) = client.connect_to(device.clone()).await?;
    Ok((events, Arc::new(handle), device))
}

fn host_unix_seconds() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

// ── scan ──────────────────────────────────────────────────────────────────────

async fn cmd_scan(args: &Args) -> Result<()> {
    let client = WhoopClient::new(config(args));
    let found = client.scan_all().await?;
    if found.is_empty() {
        println!("no WHOOP straps found in {} s", args.scan_secs);
        return Ok(());
    }
    println!("{:<22} {:<14} ID", "NAME", "FAMILY");
    for device in &found {
        println!("{}", device.label());
    }
    Ok(())
}

// ── info ──────────────────────────────────────────────────────────────────────

async fn cmd_info(args: &Args) -> Result<()> {
    let (mut events, handle, device) = open_live(args).await?;
    // Give the strap a moment to answer the handshake before asking it things.
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!();
    println!("strap      {} ({})", device.name, short_id(&device.id));
    println!("family     {}", handle.family().label());
    if handle.family() == DeviceFamily::Whoop5 {
        // Printed even when unresolved: "unknown" is the honest answer, and it
        // does not mean the strap lacks MG hardware.
        println!("variant    {:?}", handle.variant());
    }

    match handle.battery().await {
        Ok(percent) => println!("battery    {percent:.0}%"),
        Err(e) => println!("battery    unavailable ({e})"),
    }

    if handle.family() == DeviceFamily::Whoop5 {
        match handle.hello().await {
            Ok(hello) => {
                println!("firmware   {}", hello.firmware);
                println!(
                    "clock      {} ({})",
                    hello.unix_seconds,
                    clock_comment(hello.unix_seconds)
                );
                if hello.preparation_bits != 0 {
                    // Success with bits set is a documented combination, so it
                    // is reported rather than hidden behind the success.
                    println!(
                        "warning    preparation bits {:#x} set — some hello fields may be invalid",
                        hello.preparation_bits
                    );
                }
            }
            Err(e) => println!("firmware   unavailable ({e})"),
        }
    }

    match handle.data_range().await {
        Ok(range) => {
            println!(
                "history    {} page(s) behind of {} capacity, ~{} record(s) estimated",
                range.pages_behind(),
                range.capacity,
                range.record_estimate
            );
            match (range.oldest_unix(), range.newest_unix()) {
                (Some(oldest), Some(newest)) => {
                    println!("oldest     {oldest} ({})", clock_comment(oldest));
                    println!("newest     {newest} ({})", clock_comment(newest));
                }
                // Every clock pair was a sentinel or unset. Saying so beats
                // printing 1970.
                _ => println!("window     no usable timestamp in the range reply"),
            }
        }
        Err(e) => println!("history    range unavailable ({e})"),
    }

    // Drain whatever arrived while we were asking — events, logs, a pushed
    // battery reading. A strap that pushes nothing here is not broken.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.recv().await {
            if let WhoopEvent::Event(e) = &event {
                println!("event      {} at {}", e.name, e.unix_seconds);
            }
        }
    })
    .await;

    handle.disconnect().await?;
    Ok(())
}

/// A plausibility note for a strap clock reading.
///
/// A strap whose clock was never set, or which lost its RTC, reports a number
/// that looks like a date. Comparing against this host is the only check
/// available — there is no validity flag on the wire.
fn clock_comment(unix: u32) -> String {
    let now = host_unix_seconds();
    let skew = i64::from(now) - i64::from(unix);
    match skew.abs() {
        0..=120 => "in step with this host".into(),
        _ if unix < 1_293_840_001 => "implausible — clock probably never set".into(),
        s if skew > 0 => format!("{} behind this host", humanise(s as u64)),
        s => format!("{} ahead of this host", humanise(s as u64)),
    }
}

fn humanise(seconds: u64) -> String {
    match seconds {
        0..=119 => format!("{seconds} s"),
        120..=7199 => format!("{} min", seconds / 60),
        7200..=172_799 => format!("{} h", seconds / 3600),
        _ => format!("{} days", seconds / 86400),
    }
}

// ── stream ────────────────────────────────────────────────────────────────────

async fn cmd_stream(args: &Args) -> Result<()> {
    let (mut events, handle, _label) = open(args).await?;
    println!("streaming — Ctrl-C to stop\n");

    let deadline = args.duration.map(Duration::from_secs);
    let pump = async {
        let mut rejected = 0u64;
        while let Some(event) = events.recv().await {
            match &event {
                WhoopEvent::HeartRate(hr) => {
                    let rr = if hr.rr_ms.is_empty() {
                        String::new()
                    } else {
                        format!("  rr {:?} ms", hr.rr_ms)
                    };
                    println!("hr      {:>3} bpm{rr}  [{:?}]", hr.bpm, hr.source);
                }
                WhoopEvent::Battery(b) => {
                    let volts = b
                        .millivolts
                        .map(|mv| format!("  {mv} mV"))
                        .unwrap_or_default();
                    println!("battery {:>5.1}%{volts}  [{:?}]", b.percent, b.source);
                }
                WhoopEvent::Event(e) => println!("event   {} ({})", e.name, e.number),
                WhoopEvent::ConsoleLog(text) => println!("log     {}", text.trim_end()),
                WhoopEvent::Record(record) => println!("record  {}", record.summary()),
                WhoopEvent::HistoryMeta(meta) => println!("history {:?}", meta.kind),
                WhoopEvent::CommandResponse(r) => {
                    println!("reply   {} → {:?}", r.name, r.result)
                }
                WhoopEvent::RawSensor {
                    packet_type,
                    version,
                    raw,
                } => println!(
                    "raw     type {packet_type} layout {version}, {} bytes (not decoded)",
                    raw.len()
                ),
                WhoopEvent::Rejected(frame) => {
                    rejected += 1;
                    // Counted and summarised rather than printed per frame: a
                    // desynced link produces these in bursts, and a flood of
                    // identical lines hides everything else.
                    if rejected <= 3 || rejected.is_multiple_of(100) {
                        println!(
                            "bad     {:?}, {} bytes (#{rejected})",
                            frame.reason,
                            frame.raw.len()
                        );
                    }
                }
                WhoopEvent::Hello(h) => println!("hello   firmware {}", h.firmware),
                WhoopEvent::DataRange(r) => {
                    println!("range   {} page(s) behind", r.pages_behind())
                }
                WhoopEvent::Connected { name, family } => {
                    println!("linked  {name} as {}", family.label())
                }
                WhoopEvent::Disconnected => {
                    println!("\ndisconnected");
                    break;
                }
            }
        }
    };

    match deadline {
        Some(window) => {
            let _ = tokio::time::timeout(window, pump).await;
            println!("\nstopped after {} s", window.as_secs());
        }
        None => pump.await,
    }

    if args.realtime {
        // Asked for it, so turn it off again: leaving a strap streaming is a
        // battery cost the next user of it did not choose.
        let _ = handle.toggle_realtime_hr(false).await;
    }
    handle.disconnect().await?;
    Ok(())
}

// ── history ───────────────────────────────────────────────────────────────────

/// Offload the ring buffer to NDJSON, acknowledging only what reached the disk.
///
/// The order below is the whole point of the command:
///
/// 1. write the chunk's records
/// 2. flush the buffer and `fsync` the file
/// 3. only then take the acknowledgement token and send it
///
/// Any failure in 1 or 2 skips 3 and ends the session. The strap keeps the data
/// and sends it again next time, which is the outcome worth having — a clean
/// exit that silently told the strap to discard records that never landed is
/// the failure this ordering exists to prevent.
async fn cmd_history(args: &Args) -> Result<()> {
    let (mut events, handle, label) = open(args).await?;

    if args.set_clock {
        // Before the offload, not after: records are stamped with the strap's
        // clock, so a clock set afterwards does not fix what was already
        // written. Reading it back is the only confirmation available.
        match &handle {
            Strap::Live(live) => match live.set_clock(host_unix_seconds()).await {
                Ok(()) => println!("clock set from this host"),
                Err(e) => println!("clock not set ({e}) — records keep the strap's own time"),
            },
            Strap::Sim(_) => println!("--set-clock has nothing to set on a simulated strap"),
        }
    }

    let mut out = NdjsonSink::create(&args.out)?;
    println!("writing to {}", out.path().display());

    let mut session = HistorySession::new(handle.family());
    handle.begin_history().await?;
    println!("offload requested — this can take a few minutes\n");

    let mut chunks = 0u64;
    let mut written = 0u64;
    // The idle watchdog's anchor. Advanced only by offload progress — not by
    // every arriving event — because the standard Heart Rate Service notifies
    // about once a second whether or not history is moving, and a deadline
    // re-armed by that would never fire on a strap that went quiet mid-transfer.
    let mut last_progress = Instant::now();
    let outcome: Result<&str> = loop {
        let waited = last_progress.elapsed();
        if waited >= IDLE_TIMEOUT {
            break Ok("strap went silent");
        }
        let next = match tokio::time::timeout(IDLE_TIMEOUT - waited, events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => break Ok("stream closed"),
            Err(_) => break Ok("strap went silent"),
        };
        if HistorySession::is_progress(&next) {
            last_progress = Instant::now();
        }

        if let WhoopEvent::Rejected(frame) = &next {
            // Still recorded: a frame that failed its checksum is the one
            // artefact that can explain a bad link, and the strap is about to
            // be told it may discard the records around it.
            out.write_rejected(frame)?;
        }

        match session.feed(&next) {
            Step::Started => println!("stream started"),
            Step::Chunk(chunk) => {
                chunks += 1;
                let count = chunk.len();

                // ── 1 and 2: durable, or no acknowledgement ────────────────
                if let Err(e) = out.write_chunk(&chunk) {
                    break Err(e.context(
                        "could not store the chunk, so it was not acknowledged — \
                         the strap keeps it for the next attempt",
                    ));
                }
                written += count as u64;

                // ── 3: the token, now that the chunk is on disk ────────────
                match chunk.committed() {
                    Some(ack) => {
                        handle.ack_history(&ack).await?;
                        println!(
                            "chunk {chunks}: {count} record(s) stored and acknowledged \
                             (cursor {:?})",
                            ack.trim_cursor()
                        );
                    }
                    None => println!(
                        "chunk {chunks}: {count} record(s) stored, but the end marker \
                         carried no token — not acknowledged"
                    ),
                }
            }
            Step::Complete => break Ok("strap reported the attempt complete"),
            Step::Idle => {}
        }
    };

    let rejected = out.rejected();
    out.finish()?;
    let reason = match outcome {
        Ok(reason) => reason,
        Err(e) => {
            // Ask the strap to stop before leaving, so it is not left streaming
            // into a closed file.
            let _ = handle.abort_history().await;
            let _ = handle.disconnect().await;
            return Err(e);
        }
    };

    println!();
    println!("{reason}");
    println!(
        "{} record(s) in {} chunk(s) from {} → {}",
        written,
        chunks,
        label,
        args.out.display()
    );
    if rejected > 0 {
        // Kept rather than dropped: a frame that failed its checksum is the one
        // artefact that can explain a bad link.
        println!("{rejected} frame(s) failed their envelope check and were recorded");
    }
    if session.unknown_records() > 0 {
        // A different outcome from a clean run, and it should not look the
        // same. The bytes are in the file, inside the records themselves.
        println!(
            "{} record(s) had a layout this build does not decode and kept their bytes",
            session.unknown_records()
        );
    }
    // Completion is not proof that everything arrived, so the remaining backlog
    // is worth printing beside it. A simulated strap declines to report one,
    // which is why this is an `if let` rather than an unwrap.
    if let Ok(range) = handle.data_range().await {
        println!("{} page(s) still behind on the strap", range.pages_behind());
    }
    if handle.is_simulated() {
        println!("this was a simulated strap — none of it is a measurement");
    }

    handle.disconnect().await?;
    Ok(())
}

// ── buzz ──────────────────────────────────────────────────────────────────────

async fn cmd_buzz(args: &Args) -> Result<()> {
    let (_events, handle, label) = open(args).await?;
    handle.buzz(1).await?;
    println!("buzzed {label}");
    // The haptic is fired by the strap after the write is acknowledged, so
    // disconnecting immediately can cut it short.
    tokio::time::sleep(Duration::from_millis(800)).await;
    handle.disconnect().await?;
    Ok(())
}

// ── config ────────────────────────────────────────────────────────────────────

async fn cmd_config(args: &Args) -> Result<()> {
    let sub = args.arg.as_deref().unwrap_or("keys");
    let (_events, handle, _device) = open_live(args).await?;

    match sub {
        "keys" | "flags" => {
            let device_namespace = sub == "keys";
            let walk = if device_namespace {
                handle.enumerate_device_keys().await?
            } else {
                handle.enumerate_feature_flags().await?
            };
            println!();
            println!("announced {} entr(ies)", walk.announced);
            for entry in &walk.entries {
                let name = entry.key.as_deref().unwrap_or("—");
                // The known inventory is printed beside what the strap served,
                // because a name this build has never heard of is the
                // interesting case and should be visible as such.
                let known = if device_namespace {
                    DEVICE_KEYS
                        .iter()
                        .any(|k| Some(k.name) == entry.key.as_deref())
                } else {
                    FEATURE_FLAGS
                        .iter()
                        .any(|f| Some(f.name) == entry.key.as_deref())
                };
                println!(
                    "  {:>3}  {}  {:<34} {}",
                    entry.index,
                    if entry.valid { "ok  " } else { "slot" },
                    name,
                    if known {
                        ""
                    } else {
                        "(not in this build's inventory)"
                    }
                );
            }
            // Named, because only the first is the strap saying it is finished;
            // the rest are this side's limits and must not read as a complete
            // list.
            println!("stop: {:?}", walk.stop);
        }
        "get" => {
            let key = args
                .value
                .as_deref()
                .ok_or_else(|| anyhow!("config get needs a key name"))?;
            // Which namespace a name belongs to comes from the inventory, so a
            // caller does not have to know which command reads it.
            let value = if DEVICE_KEYS.iter().any(|k| k.name == key) {
                handle.read_device_value(key).await?
            } else if FEATURE_FLAGS.iter().any(|f| f.name == key) {
                handle.read_feature_flag(key).await?
            } else {
                return Err(anyhow!(
                    "{key:?} is in neither inventory; `config keys` and `config flags` list them"
                ));
            };
            println!();
            println!("{} = {:?}", value.key, value.value);
            if value.zero_filled {
                // Result 1 with zeros can be an unreadable storage slot, so
                // this is not the same as "the stored value is zero".
                println!("note: the value field was all zeros — this may be a read fallback");
            }
        }
        "set" => {
            let key = args
                .value
                .as_deref()
                .ok_or_else(|| anyhow!("config set needs a key and a value"))?;
            let value = args
                .extra
                .as_deref()
                .ok_or_else(|| anyhow!("config set needs a value after the key"))?;
            if DEVICE_KEYS.iter().any(|k| k.name == key) {
                handle.write_device_value(key, value).await?;
            } else if FEATURE_FLAGS.iter().any(|f| f.name == key) {
                handle.write_feature_flag(key, value).await?;
            } else {
                return Err(anyhow!("{key:?} is in neither inventory"));
            }
            println!("wrote {key} = {value:?}");
            // Four separate states, and the write only proves the first.
            println!("note: acceptance is not readback, visible effect or reboot persistence");
            match handle.read_device_value(key).await {
                Ok(back) => println!("readback: {} = {:?}", back.key, back.value),
                Err(e) => println!("readback unavailable ({e})"),
            }
        }
        "disable-r22" => {
            let unconfirmed = handle.disable_r22().await?;
            if unconfirmed.is_empty() {
                println!("every R22 flag read back as cleared");
            } else {
                // Not a transport failure: the strap disagreed with its own
                // acknowledgement, which is worth saying plainly.
                println!("these flags did not confirm the write:");
                for key in unconfirmed {
                    println!("  {key}");
                }
            }
        }
        other => {
            return Err(anyhow!(
                "unknown config subcommand {other:?}; try keys, flags, get, set or disable-r22"
            ))
        }
    }

    handle.disconnect().await
}

// ── alarm ─────────────────────────────────────────────────────────────────────

async fn cmd_alarm(args: &Args) -> Result<()> {
    let sub = args.arg.as_deref().unwrap_or("get");
    let (_events, handle, _device) = open_live(args).await?;
    // WHOOP 5 has six slots; WHOOP 4 has one and ignores the ID.
    let id: u8 = args
        .value
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|_| anyhow!("alarm IDs are numbers 1–6"))?
        .unwrap_or(1);

    match sub {
        "get" => {
            let record = handle.get_alarm(id).await?;
            println!();
            if record.is_set() {
                println!(
                    "alarm {} at {} ({})",
                    record.id,
                    record.unix_seconds,
                    clock_comment(record.unix_seconds)
                );
                println!(
                    "pattern: {} repeat(s), duration {}, crescendo {}",
                    record.repeats, record.duration, record.crescendo
                );
            } else {
                // Zeros mean a cleared slot *or* a read fallback, and the wire
                // carries no flag separating them.
                println!(
                    "alarm {} has no stored time (cleared, or a read fallback)",
                    record.id
                );
            }
        }
        "set" => {
            let epoch_ms: i64 = args
                .value
                .as_deref()
                .ok_or_else(|| anyhow!("alarm set needs an absolute epoch in milliseconds"))?
                .parse()
                .map_err(|_| anyhow!("the wake instant must be epoch milliseconds"))?;
            let detail = handle
                .arm_alarm(id, epoch_ms, HapticPattern::default())
                .await?;
            println!();
            println!("validation detail: {detail:?}");
            // One-shot by design: a due alarm is cleared before the haptic
            // finishes, so recurrence is the caller's job.
            println!("note: this is a one-shot schedule, not a daily recurrence");
            match handle.get_alarm(id).await {
                Ok(back) if back.is_set() => println!("readback: armed for {}", back.unix_seconds),
                Ok(_) => println!("readback: the slot is empty — the time was probably rejected"),
                Err(e) => println!("readback unavailable ({e})"),
            }
        }
        "run" => {
            let detail = handle.run_alarm(id).await?;
            println!("run detail: {detail:?}");
            println!("note: RUN clears the slot it fired");
        }
        "off" => {
            // No ID given means every slot, which can partly succeed.
            let target = args.value.as_deref().map(|_| id);
            handle.disable_alarm(target).await?;
            match target {
                Some(id) => println!("alarm {id} disarmed"),
                None => println!("all alarm slots disarmed"),
            }
        }
        "stop" => {
            handle.stop_haptics().await?;
            println!("stop requested");
            println!("note: this stops the current effect; stored alarms stay armed");
        }
        other => {
            return Err(anyhow!(
                "unknown alarm subcommand {other:?}; try get, set, run, off or stop"
            ))
        }
    }

    handle.disconnect().await
}

// ── probe ─────────────────────────────────────────────────────────────────────

async fn cmd_probe(args: &Args) -> Result<()> {
    let sub = args.arg.as_deref().unwrap_or("location");
    let (_events, handle, _device) = open_live(args).await?;

    match sub {
        "location" => {
            let status = handle.body_location().await?;
            println!();
            match status.location {
                Some(location) => println!("location: {location:?}"),
                None => println!("location: not decoded on this family"),
            }
            match status.on_body {
                Some(on) => println!("on body:  {on}"),
                None => println!("on body:  not reported"),
            }
            println!("raw:      {:02x?}", status.raw);
            println!("note: diagnostic only — this never feeds wear or sleep logic");
        }
        "battery-pack" => {
            let pack = handle.battery_pack().await?;
            println!();
            println!("present:    {}", pack.present);
            println!("serial:     {}", pack.serial);
            // Unscaled on purpose: the wire does not establish a percentage.
            println!("charge raw: {} (no established scale)", pack.charge_raw);
        }
        "battery-ext" => {
            let battery = handle.extended_battery().await?;
            println!();
            println!("pack voltage: {} mV", battery.millivolts);
            println!("record:       {:02x?}", battery.raw);
        }
        "name" => {
            let name = handle.advertising_name().await?;
            println!();
            println!("name:   {:?}", name.name);
            println!(
                "status: {} ({})",
                name.status,
                if name.custom {
                    "custom"
                } else {
                    "fallback, empty or unreadable"
                }
            );
        }
        "serial" => {
            println!();
            // Two sightings by design — see WhoopHandle::whoop4_serial.
            match handle.whoop4_serial().await? {
                Some(serial) => println!("serial: {serial}"),
                None => {
                    println!("first sighting recorded; asking again to confirm…");
                    match handle.whoop4_serial().await? {
                        Some(serial) => println!("serial: {serial} (confirmed by two hellos)"),
                        None => {
                            println!("the two hellos did not agree — identity stays unresolved")
                        }
                    }
                }
            }
        }
        "reboot" => {
            let which: usize = args
                .value
                .as_deref()
                .ok_or_else(|| anyhow!("probe reboot needs a candidate number, 1–5"))?
                .parse()
                .map_err(|_| anyhow!("the candidate must be 1–5"))?;
            let variant = RebootProbe::ALL
                .get(
                    which
                        .checked_sub(1)
                        .ok_or_else(|| anyhow!("candidates start at 1"))?,
                )
                .copied()
                .ok_or_else(|| anyhow!("there are five candidates"))?;
            println!();
            println!("sending {} — this restarts the strap", variant.label());
            handle.reboot_probe(variant).await?;
            println!("sent. Watch whether the link drops or the request is ignored.");
            return Ok(());
        }
        other => {
            return Err(anyhow!(
                "unknown probe subcommand {other:?}; try location, battery-pack, \
                 battery-ext, name, serial or reboot"
            ))
        }
    }

    handle.disconnect().await
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Called from `main.rs`, which is the binary's entry point; this is the body.
#[tokio::main]
pub async fn run() -> Result<()> {
    env_logger::init();
    let args = parse_args()?;
    match args.command {
        Command::Scan => cmd_scan(&args).await,
        Command::Info => cmd_info(&args).await,
        Command::Stream => cmd_stream(&args).await,
        Command::History => cmd_history(&args).await,
        Command::Buzz => cmd_buzz(&args).await,
        Command::Config => cmd_config(&args).await,
        Command::Alarm => cmd_alarm(&args).await,
        Command::Probe => cmd_probe(&args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_keeps_both_ends() {
        // A truncated prefix does not distinguish two straps paired to the same
        // machine, so the tail is kept too.
        let id = "1A2B3C4D-5E6F-7081-9203-A4B5C6D7E8F9";
        let short = short_id(id);
        assert!(short.starts_with("1A2B3C"));
        assert!(short.ends_with("D7E8F9"));
        // Something already short is left alone.
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn clock_comment_flags_a_clock_that_was_never_set() {
        // Below the documented threshold for a stored time to be applied.
        assert!(clock_comment(1_000_000_000).contains("never set"));
        assert!(clock_comment(host_unix_seconds()).contains("in step"));
    }

    #[test]
    fn humanise_picks_a_readable_unit() {
        assert_eq!(humanise(45), "45 s");
        assert_eq!(humanise(600), "10 min");
        assert_eq!(humanise(10_800), "3 h");
        assert_eq!(humanise(259_200), "3 days");
    }
}
