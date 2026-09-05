//! Records an audio input to a Broadcast Wave file, stamped with the timecode a
//! Tentacle is broadcasting over Bluetooth.
//!
//! The two halves of this crate meeting: [`shokushu::ble`] says what time it is
//! and [`shokushu::audio`] brings in the samples, and what comes out is a file
//! that drops onto somebody else's timeline in the right place. No cable
//! between the Tentacle and the computer, and nothing typed in afterwards.
//!
//! ```console
//! $ shokushu-rec --name ricki --device external
//! waiting for Ricki…  locked at 11:37:38:04, 25 fps, 2026-09-04
//! ● 00:01:23.4   11:39:01:19   25 fps   Ricki   -18.3 dBFS   ricki_2026-09-04_11-37-40-21.wav
//! ```
//!
//! # Where the accuracy comes from, and where it stops
//!
//! The file's start time is not the timecode that happened to be on screen when
//! recording began. It is that clock extrapolated back to the instant the first
//! sample was *captured* — which is not the instant the callback holding it ran.
//! `cpal` reports both, and the difference between them is a buffer plus the
//! driver's own latency: at 512 frames and 48 kHz that is over 10 ms of head
//! start, a quarter of a frame at 25 fps, all of it in the same direction. See
//! [`captured_at`].
//!
//! `bext`'s `TimeReference` is then written in samples rather than frames, so
//! the sub-frame field in the advertisement — good to about 0.6 ms, per
//! `PROTOCOL.md` — survives into the file instead of being rounded to the 40 ms
//! frame it fell in.
//!
//! What that buys is a start time good to a millisecond or two, which is under
//! a tenth of a frame. What it does not buy is *clocking*: the audio interface
//! keeps its own time once recording starts, and nothing here steers it. Over a
//! long take the two drift apart, which is the last line this prints — see
//! [`report_drift`] — and if that matters, the answer is LTC on a track, not
//! Bluetooth.
//!
//! # What it does when things go wrong
//!
//! Timecode is required to *start* and not to continue. Losing the signal
//! mid-take is a non-event: the stamp was written when the file was opened, and
//! the recording carries on. What it will not do is start without a reading,
//! since a file stamped with a guess is worse than one that says nothing.
//!
//! Ctrl-C is a clean stop, not an abort. The two size fields at the top of the
//! file are all that separate a finished recording from an interrupted one, and
//! they get written.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use btleplug::platform::PeripheralId;
use clap::Parser;
use cpal::traits::{DeviceTrait, StreamTrait};

use shokushu::audio;
use shokushu::ble::diagnostics::Diagnosis;
use shokushu::ble::{self, Event, Scanner};
use shokushu::freerun::Reading;
use shokushu::wav::{self, Bwf, Spec, Stamp};
use shokushu::Timecode;

/// How often to redraw the recording line.
const TICK: Duration = Duration::from_millis(100);

/// How long to let a date advertisement turn up before writing the header
/// without one.
///
/// The date rides in its own record and comes round far more rarely than the
/// timecode — about one in nineteen payloads across the captures behind
/// `PROTOCOL.md`, so a few seconds apart. It fills in `bext`'s OriginationDate,
/// which cannot be added once the header is down.
///
/// This wait costs no audio. It happens after the input is already running, so
/// the samples are being captured and queued the whole time it lasts; what is
/// waiting is the file, not the recording. That is the only reason it is as
/// long as this — before the stream was opened, the same wait would have been
/// five seconds of the take that never happened.
const DATE_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the audio device's first buffer before deciding it is
/// never coming. Generous: a device that has to be woken can take a moment.
const FIRST_BUFFER: Duration = Duration::from_secs(5);

/// How far out an anchor can be, for the uncertainty on the drift figure at the
/// end. `PROTOCOL.md` puts a reading at about 0.6 ms against host arrival
/// times; a millisecond an end is the round number above that.
const ANCHOR_ERROR: Duration = Duration::from_millis(1);

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Record an audio input to a BWF stamped with a Tentacle's timecode"
)]
struct Opt {
    /// Where to write. Defaults to a name built from the device, the date and
    /// the timecode of the first sample.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Which Tentacle to take timecode from: part of its Bluetooth name.
    /// Defaults to the first one that speaks up.
    #[arg(short, long)]
    name: Option<String>,

    /// Input device: a device id, or part of a device name. Defaults to the
    /// system default input.
    #[arg(short, long)]
    device: Option<String>,

    /// List the available input devices and exit.
    #[arg(short, long)]
    list_devices: bool,

    /// Stop after this many seconds. 0 records until interrupted.
    #[arg(short, long, default_value_t = 0)]
    seconds: u64,

    /// Give up if no timecode has arrived within this many seconds.
    #[arg(long, default_value_t = 15)]
    wait: u64,
}

/// Why the recording stopped, which is worth saying since one of them is an
/// error and the other two are not.
enum Stopped {
    Interrupted,
    Elapsed,
    ScanEnded,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    let host = cpal::default_host();

    if opt.list_devices {
        audio::list_devices(&host)?;
        return Ok(());
    }

    let device = audio::open_device(&host, opt.device.as_deref())?;
    let config = device
        .default_input_config()
        .context("failed to read the device's default input config")?;
    let spec = Spec {
        sample_rate: config.sample_rate(),
        channels: config.channels(),
    };
    let sample_rate = spec.sample_rate as f64;
    let channels = spec.channels.max(1) as usize;

    let mut builder = Scanner::builder();
    if let Some(name) = &opt.name {
        builder = builder.name(name);
    }
    let mut scan = builder.start().await?;
    eprintln!(
        "adapter state: {:?} — recording from {} ({} Hz, {} ch)",
        scan.adapter_state().await?,
        audio::describe(&device),
        spec.sample_rate,
        spec.channels
    );

    // Timecode first. Opening the input is cheap and reversible; a file stamped
    // with a timecode nobody sent is not, so nothing is recorded until a box
    // has been heard from.
    let id = lock_on(&mut scan, &opt).await?;
    let name = scan
        .device(&id)
        .and_then(|d| d.name())
        .unwrap_or("<unnamed>")
        .to_string();

    // Samples out of the callback and empty buffers back to it. Returning them
    // is what keeps the callback from allocating once the recording is going:
    // it is a realtime thread, and an allocator is the classic way to make one
    // miss its deadline.
    let (full_tx, full_rx) = mpsc::channel::<Vec<f32>>();
    let (empty_tx, empty_rx) = mpsc::channel::<Vec<f32>>();

    // When the very first sample was captured, and how far into the recording
    // the most recent buffer ended — both on the host clock, both set by the
    // audio thread and read here.
    let started: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let captured_ns = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU32::new(0));

    let on_samples = {
        let started = started.clone();
        let captured_ns = captured_ns.clone();
        let peak = peak.clone();
        move |samples: &[f32], info: &cpal::InputCallbackInfo| {
            let frames = samples.len() / channels;
            let captured = captured_at(info, frames, sample_rate);
            let begun = *started.get_or_init(|| captured);
            let ends = captured + Duration::from_secs_f64(frames as f64 / sample_rate);
            captured_ns.store(
                ends.saturating_duration_since(begun).as_nanos() as u64,
                Ordering::Relaxed,
            );

            let loudest = samples.iter().fold(0.0f32, |max, s| max.max(s.abs()));
            // Hold the peak and let it fall, so a meter read every 100 ms sees
            // what happened in between rather than whichever buffer it landed on.
            let held = f32::from_bits(peak.load(Ordering::Relaxed));
            peak.store((held * 0.85).max(loudest).to_bits(), Ordering::Relaxed);

            let mut buffer = empty_rx.try_recv().unwrap_or_default();
            buffer.clear();
            buffer.extend_from_slice(samples);
            // A closed channel means the writer has gone, which means we're
            // shutting down.
            let _ = full_tx.send(buffer);
        }
    };

    let err_fn = |err: cpal::Error| eprintln!("\nstream error: {err}");
    let stream = audio::open_input_stream(&device, config, on_samples, err_fn)?;
    stream.play().context("failed to start the input stream")?;

    // The file can't be opened until the first buffer has been handed over,
    // because its name and its stamp both depend on when that buffer's first
    // sample was captured. That is a wait of one buffer period.
    let first = full_rx
        .recv_timeout(FIRST_BUFFER)
        .context("the audio device delivered nothing")?;
    let begun = *started
        .get()
        .expect("the callback sets this before sending a buffer");

    let (start, samples_at_start, start_position) =
        start_timecode(&mut scan, &id, begun, spec.sample_rate)?;

    // Only now, with the input running and its samples piling up behind us,
    // is it worth standing still for the date.
    let date = wait_for_date(&mut scan, &id).await;
    let path = opt
        .output
        .clone()
        .unwrap_or_else(|| default_path(&name, date.as_deref(), &start));

    let bwf = Bwf::create(
        &path,
        spec,
        &Stamp {
            timecode: start,
            samples_since_midnight: samples_at_start,
            date: date.clone(),
            originator: "shokushu-rec".to_string(),
            note: format!("timecode from {name} over BLE"),
        },
    )
    .with_context(|| format!("failed to create {}", path.display()))?;

    eprintln!(
        "first sample at {start} {} — writing {}",
        start.rate,
        path.display()
    );

    let recorded = Arc::new(AtomicU64::new(0));
    let writer = {
        let recorded = recorded.clone();
        std::thread::spawn(move || -> Result<Bwf> {
            let mut bwf = bwf;
            bwf.write(&first)?;
            recorded.store(bwf.frames(), Ordering::Relaxed);
            while let Ok(chunk) = full_rx.recv() {
                bwf.write(&chunk)?;
                recorded.store(bwf.frames(), Ordering::Relaxed);
                let _ = empty_tx.send(chunk);
            }
            Ok(bwf)
        })
    };

    let stopped = record(
        &mut scan, &id, &opt, &name, begun, &recorded, &peak, sample_rate,
    )
    .await;

    // Dropping the stream stops the callbacks, which drops the sender inside
    // it, which is what ends the writer's loop. Order matters: joining first
    // would wait forever.
    drop(stream);
    let bwf = writer
        .join()
        .map_err(|_| anyhow::anyhow!("the writer thread panicked"))?
        .context("failed while writing samples")?;

    let frames = bwf.frames();
    let clipped = bwf.clipped();
    let recorded_for = Duration::from_nanos(captured_ns.load(Ordering::Relaxed));
    bwf.finish().context("failed to finish the file")?;

    eprint!("\r\x1b[K");
    match stopped {
        Stopped::Interrupted | Stopped::Elapsed => {}
        Stopped::ScanEnded => eprintln!("the bluetooth event stream ended"),
    }
    eprintln!(
        "wrote {} — {:.3} s, {} frames at {} Hz, {} ch, 24-bit",
        path.display(),
        frames as f64 / sample_rate,
        frames,
        spec.sample_rate,
        spec.channels
    );
    eprintln!("  first sample at {start} {}, {samples_at_start} samples since midnight{}",
        start.rate,
        match &date {
            Some(date) => format!(", {date}"),
            None => ", no date received".to_string(),
        }
    );
    if clipped > 0 {
        eprintln!("  {clipped} samples clipped");
    }
    report_drift(
        &mut scan,
        &id,
        &Take {
            begun,
            start_position,
            recorded_for,
            frames,
            sample_rate,
            name,
        },
    );

    scan.stop().await?;
    Ok(())
}

/// Waits for a Tentacle to say what time it is, and takes the first one that
/// does as the device to record against.
///
/// The scanner is already filtering on `--name`, so anything that reaches here
/// is a box we were willing to use. Without a filter that means the first one
/// to speak up, which in a room with two of them is a coin toss — hence the
/// line saying which one it took.
async fn lock_on(scan: &mut Scanner, opt: &Opt) -> Result<PeripheralId> {
    let mut deadline = std::pin::pin!(tokio::time::sleep(Duration::from_secs(opt.wait)));

    let id = loop {
        tokio::select! {
            _ = &mut deadline => {
                // The scan knows why it found nothing better than we do.
                bail!(
                    "no timecode after {}s \u{2014} {}",
                    opt.wait,
                    why(&scan.diagnosis(), opt.name.as_deref())
                );
            }
            event = scan.next() => match event {
                Some(Event::Timecode { id, .. }) => break id,
                Some(_) => continue,
                None => bail!("the bluetooth event stream ended before any timecode arrived"),
            },
        }
    };

    let device = scan.device_mut(&id).expect("it just sent us timecode");
    let name = device.name().unwrap_or("<unnamed>").to_string();
    match device.last_received() {
        Some(tc) => eprintln!("locked onto {name} at {tc} {}", tc.rate),
        None => bail!("{name} sent timecode and then its clock had none"),
    }
    Ok(id)
}

/// The date the device is set to, waiting [`DATE_GRACE`] for one if it hasn't
/// said yet.
///
/// Returns `None` rather than guessing. The host's own date is not an answer: a
/// Tentacle can be set to a different one, and a file that states the wrong day
/// with authority is worse than one that states none.
async fn wait_for_date(scan: &mut Scanner, id: &PeripheralId) -> Option<String> {
    if let Some(date) = scan.device(id).and_then(|d| d.date()) {
        return Some(date.to_string());
    }

    let mut grace = std::pin::pin!(tokio::time::sleep(DATE_GRACE));
    loop {
        tokio::select! {
            _ = &mut grace => return None,
            event = scan.next() => match event {
                Some(Event::Date { id: from, date }) if from == *id => {
                    return Some(date.to_string())
                }
                Some(_) => continue,
                None => return None,
            },
        }
    }
}

/// The timecode at the instant the first sample was captured, and where that
/// falls on the day in samples.
///
/// Extrapolated *backwards* from the clock as it reads now, which is a few
/// milliseconds of arithmetic on a model that is already running — not a
/// separate reading, and not the last advertisement, which may be half a second
/// old by now.
fn start_timecode(
    scan: &mut Scanner,
    id: &PeripheralId,
    begun: Instant,
    sample_rate: u32,
) -> Result<(Timecode, u64, f64)> {
    let now = Instant::now();
    let device = scan
        .device_mut(id)
        .context("the device we locked onto is no longer in the scan")?;

    let tc = match device.reading(now) {
        Some(Reading::Running(tc)) => tc,
        Some(Reading::Lost { last, since }) => bail!(
            "the timecode stopped {:.1}s ago (last was {last}); not recording against a guess",
            since.as_secs_f64()
        ),
        None => bail!("the device's clock went away between locking on and recording"),
    };

    let back = now.saturating_duration_since(begun).as_secs_f64() * tc.rate.fps as f64;
    let position = tc.frame_position() - back;
    let start = Timecode::at_frame_position(position, tc.rate);
    let samples = wav::samples_since_midnight(&start, sample_rate)
        .context("a drop-frame rate has no sample position, and Bluetooth never reports one")?;
    Ok((start, samples, position))
}

/// Runs until something stops it, keeping the clock fed and the line redrawn.
#[allow(clippy::too_many_arguments)]
async fn record(
    scan: &mut Scanner,
    id: &PeripheralId,
    opt: &Opt,
    name: &str,
    begun: Instant,
    recorded: &AtomicU64,
    peak: &AtomicU32,
    sample_rate: f64,
) -> Stopped {
    // `--seconds` is seconds of recording, counted from the first sample —
    // which is earlier than this, since the input opens before the file does
    // and keeps whatever it captures while the date is waited for. Counting
    // from here instead would hand back a take that long plus the wait.
    let mut deadline = std::pin::pin!(async {
        match opt.seconds {
            0 => std::future::pending::<()>().await,
            n => {
                let asked = Duration::from_secs(n);
                tokio::time::sleep(asked.saturating_sub(begun.elapsed())).await
            }
        }
    });
    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut deadline => break Stopped::Elapsed,
            _ = &mut interrupt => break Stopped::Interrupted,
            _ = ticker.tick() => {
                draw(scan, id, name, recorded, peak, sample_rate);
            }
            // Every advertisement goes to the device's clock on its way past,
            // which is the whole reason to keep reading events we don't use.
            event = scan.next() => {
                if event.is_none() {
                    break Stopped::ScanEnded;
                }
            }
        }
    }
}

/// The one line that updates in place while recording.
fn draw(
    scan: &mut Scanner,
    id: &PeripheralId,
    name: &str,
    recorded: &AtomicU64,
    peak: &AtomicU32,
    sample_rate: f64,
) {
    let frames = recorded.load(Ordering::Relaxed);
    let elapsed = frames as f64 / sample_rate;
    let reading = scan.device_mut(id).and_then(|d| d.reading(Instant::now()));

    // Losing the signal doesn't stop the recording — the stamp is already
    // written — so it's a note on the line rather than an error.
    let (timecode, note) = match reading {
        Some(Reading::Running(tc)) => (format!("{tc}   {}", tc.rate), String::new()),
        Some(Reading::Lost { last, since }) => (
            format!("{last}   {}", last.rate),
            format!("   no timecode for {:.0}s", since.as_secs_f64()),
        ),
        None => ("--:--:--:--".to_string(), "   no clock".to_string()),
    };

    eprint!(
        "\r● {}   {timecode}   {name}   {:>6.1} dBFS{note}\x1b[K",
        clock(elapsed),
        decibels(peak)
    );
    let _ = std::io::stderr().flush();
}

/// What was recorded, for the figure at the end.
struct Take {
    /// When the first sample was captured, on the host clock.
    begun: Instant,
    /// Where the device's clock said that was, in frames since midnight. Taken
    /// at the time, which is the whole point — see [`report_drift`].
    start_position: f64,
    /// How much host time the captured samples span.
    recorded_for: Duration,
    frames: u64,
    sample_rate: f64,
    name: String,
}

/// How far the input's clock and the Tentacle's ran apart over the take.
///
/// The two ends have to be *two readings*, taken minutes apart, and not one
/// reading extrapolated to both instants: extrapolate twice from the same model
/// and the device's clock cancels out of the subtraction exactly, leaving the
/// audio interface being compared against itself. So the start comes from
/// [`start_timecode`], which sampled the clock when the recording actually
/// began, and only the end is read here.
///
/// Both ends come off the same free-running clock, so this says nothing about
/// which of the two is right — only that they disagree, and by how much. The
/// uncertainty is what keeps that honest: an anchor is good to about a
/// millisecond, so over a short take the figure is mostly noise and over a long
/// one it isn't. Ten seconds of recording cannot measure a crystal.
fn report_drift(scan: &mut Scanner, id: &PeripheralId, take: &Take) {
    let now = Instant::now();
    let Some(Reading::Running(tc)) = scan.device_mut(id).and_then(|d| d.reading(now)) else {
        eprintln!("  no timecode at the end of the take, so no drift figure");
        return;
    };

    // Where the device's clock was when the last sample was captured, reached
    // backwards from now rather than from the last advertisement, which may be
    // half a second old.
    let ended = take.begun + take.recorded_for;
    let back = now.saturating_duration_since(ended).as_secs_f64() * tc.rate.fps as f64;
    let mut end_position = tc.frame_position() - back;
    // A take that ran through midnight ends at a smaller position than it
    // started at, the day having wrapped underneath it.
    if end_position < take.start_position {
        end_position += tc.rate.frames_per_day();
    }

    let device_seconds = (end_position - take.start_position) / tc.rate.fps as f64;
    let audio_seconds = take.frames as f64 / take.sample_rate;
    if device_seconds < 1.0 {
        return;
    }

    let ppm = (audio_seconds / device_seconds - 1.0) * 1e6;
    // One anchor error at each end, over the baseline between them.
    let uncertainty = 2.0 * ANCHOR_ERROR.as_secs_f64() / device_seconds * 1e6;
    let verdict = match ppm.abs() > uncertainty {
        true => String::new(),
        false => " \u{2014} which is not distinguishable from no difference at all".to_string(),
    };
    eprintln!(
        "  the input's clock ran {ppm:+.0} ppm against {}'s over {device_seconds:.0} s (\u{00b1}{uncertainty:.0} ppm){verdict}",
        take.name
    );
}

/// Why nothing arrived, in a sentence.
///
/// The full wording lives in `shokushu-ble`, which is the tool for looking at a
/// scan; this is the short form plus a pointer at it, since what someone
/// running a recorder wants is to know whether to keep waiting.
fn why(diagnosis: &Diagnosis, filter: Option<&str>) -> String {
    let detail = match diagnosis {
        Diagnosis::SilentScan => {
            "not one BLE advertisement of any kind has arrived, so suspect the scan rather than \
             the Tentacles"
                .to_string()
        }
        Diagnosis::FilteredOut { devices } => format!(
            "{devices} BLE devices in range, none named like {:?}",
            filter.unwrap_or_default()
        ),
        Diagnosis::NothingAdvertising { devices } => {
            format!("{devices} BLE devices in range, none of which has advertised anything")
        }
        Diagnosis::Unreadable {
            name,
            payloads,
            unparsed,
            ..
        } => format!(
            "{name} is advertising 0x{:04X}, but {unparsed} of {payloads} payloads did not decode",
            ble::SERVICE_UUID_16
        ),
        Diagnosis::DatesOnly { payloads, .. } => format!(
            "{payloads} payloads decoded and none carried timecode \u{2014} dates only so far"
        ),
        Diagnosis::NoTentacle { matched } => format!(
            "{matched} BLE devices in range, none advertising 0x{:04X}",
            ble::SERVICE_UUID_16
        ),

        // `Diagnosis` is non-exhaustive, so a case added to the library reaches
        // here before it reaches this program. Say the key rather than nothing.
        other => other.key(),
    };
    format!("{detail} (shokushu-ble shows what the scan is taking in)")
}

/// When the first sample in this buffer was captured, on the host clock.
///
/// The callback runs after the data exists, by a buffer plus whatever the
/// driver adds, and `cpal` reports both instants so the gap between them is
/// measurable rather than assumed. Ignoring it puts every recording late by the
/// same amount, which at 512 frames and 48 kHz is a quarter of a frame before
/// the driver has added anything.
///
/// The fallback — a buffer period — is what to assume when a backend won't say:
/// wrong by the driver's latency, but the right order of magnitude, and it
/// errs in the same direction rather than a random one.
fn captured_at(info: &cpal::InputCallbackInfo, frames: usize, sample_rate: f64) -> Instant {
    let timestamp = info.timestamp();
    let latency = timestamp
        .callback
        .checked_duration_since(timestamp.capture)
        .unwrap_or_else(|| Duration::from_secs_f64(frames as f64 / sample_rate));
    let now = Instant::now();
    now.checked_sub(latency).unwrap_or(now)
}

/// A default filename that says what the file is without being opened.
fn default_path(name: &str, date: Option<&str>, start: &Timecode) -> PathBuf {
    let stem = format!(
        "{}_{}_{:02}-{:02}-{:02}-{:02}.wav",
        slug(name),
        date.unwrap_or("undated"),
        start.hours,
        start.minutes,
        start.seconds,
        start.frames
    );
    PathBuf::from(stem)
}

/// A device name as something safe to put in a filename.
fn slug(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-');
    match trimmed.is_empty() {
        true => "tentacle".to_string(),
        false => trimmed.to_string(),
    }
}

/// Elapsed recording time, as a running clock rather than a count of seconds.
fn clock(seconds: f64) -> String {
    let whole = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:04.1}",
        whole / 3600,
        whole / 60 % 60,
        seconds - (whole / 60 * 60) as f64
    )
}

fn decibels(peak: &AtomicU32) -> f64 {
    let amplitude = f32::from_bits(peak.load(Ordering::Relaxed)) as f64;
    match amplitude > 0.0 {
        true => 20.0 * amplitude.log10(),
        false => f64::NEG_INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shokushu::Rate;

    #[test]
    fn a_default_name_says_what_the_file_is() {
        let start = Timecode::new(11, 37, 40, 21, Rate::whole(25));
        assert_eq!(
            default_path("Ricki", Some("2026-09-04"), &start),
            PathBuf::from("ricki_2026-09-04_11-37-40-21.wav")
        );
        // No date advertisement arrived in time, which is not a reason to
        // refuse to name the file.
        assert_eq!(
            default_path("Ricki", None, &start),
            PathBuf::from("ricki_undated_11-37-40-21.wav")
        );
    }

    #[test]
    fn a_name_becomes_something_a_filesystem_will_take() {
        assert_eq!(slug("Tentacle Sync E"), "tentacle-sync-e");
        assert_eq!(slug("a/b:c"), "a-b-c");
        assert_eq!(slug("  spaced  "), "spaced");
        // A name of nothing usable still has to produce a filename.
        assert_eq!(slug("///"), "tentacle");
        assert_eq!(slug(""), "tentacle");
    }

    #[test]
    fn the_elapsed_clock_carries_at_a_minute() {
        assert_eq!(clock(0.0), "00:00:00.0");
        assert_eq!(clock(9.4), "00:00:09.4");
        assert_eq!(clock(61.2), "00:01:01.2");
        assert_eq!(clock(3661.0), "01:01:01.0");
    }
}
