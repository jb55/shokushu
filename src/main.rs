//! Reads SMPTE LTC timecode from an audio input — e.g. a Tentacle Sync E
//! plugged into the headset jack on a Mac.

mod ltc;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample};

use ltc::{DecodedFrame, LtcDecoder};

/// How long without a frame before we call it a signal loss.
const SIGNAL_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Read SMPTE LTC timecode from an audio input (e.g. a Tentacle Sync E)"
)]
struct Opt {
    /// List the available input devices and exit.
    #[arg(short, long)]
    list_devices: bool,

    /// Input device: a device id, or part of a device name. Defaults to the
    /// system default input.
    #[arg(short, long)]
    device: Option<String>,

    /// Which channel of the input to decode.
    #[arg(short, long, default_value_t = 0)]
    channel: usize,

    /// Emit one JSON object per decoded frame instead of a live display.
    #[arg(short, long)]
    json: bool,
}

fn main() -> Result<()> {
    let opt = Opt::parse();
    let host = cpal::default_host();

    if opt.list_devices {
        return list_devices(&host);
    }

    let device = match &opt.device {
        Some(want) => find_device(&host, want)?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };

    let config = device
        .default_input_config()
        .context("failed to read the device's default input config")?;
    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate() as f64;

    if opt.channel >= channels {
        return Err(anyhow!(
            "channel {} was requested but the device only has {} ({})",
            opt.channel,
            channels,
            (0..channels)
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    eprintln!(
        "listening on {} ({} Hz, {} ch, {}), channel {}",
        describe(&device),
        sample_rate,
        channels,
        config.sample_format(),
        opt.channel
    );

    let (tx, rx) = mpsc::channel::<DecodedFrame>();
    // The audio callback must not block, so the level meter rides along in an
    // atomic rather than through the channel.
    let level = Arc::new(AtomicU32::new(0));

    let mut decoder = LtcDecoder::new(sample_rate);
    let mut mono: Vec<f32> = Vec::new();
    let mut frames: Vec<DecodedFrame> = Vec::new();
    let meter = level.clone();
    let channel = opt.channel;

    let on_samples = move |samples: &[f32]| {
        mono.clear();
        mono.extend(samples.iter().skip(channel).step_by(channels));
        frames.clear();
        decoder.process(&mono, &mut frames);
        meter.store((decoder.level() as f32).to_bits(), Ordering::Relaxed);
        for frame in frames.drain(..) {
            // A closed channel just means main is on its way out.
            let _ = tx.send(frame);
        }
    };

    let err_fn = |err: cpal::Error| eprintln!("stream error: {err}");
    let stream_config = config.into();
    let stream = match config.sample_format() {
        SampleFormat::I8 => build::<i8, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::I16 => build::<i16, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::I32 => build::<i32, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::U8 => build::<u8, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::U16 => build::<u16, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::F32 => build::<f32, _, _>(&device, stream_config, on_samples, err_fn),
        SampleFormat::F64 => build::<f64, _, _>(&device, stream_config, on_samples, err_fn),
        other => return Err(anyhow!("unsupported sample format {other}")),
    }
    .context("failed to open the input stream")?;

    stream.play().context("failed to start the input stream")?;

    report(rx, &level, opt.json, sample_rate);
    Ok(())
}

fn build<T, D, E>(
    device: &Device,
    config: cpal::StreamConfig,
    mut on_samples: D,
    err_fn: E,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
    D: FnMut(&[f32]) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let mut buf: Vec<f32> = Vec::new();
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _: &_| {
            buf.clear();
            buf.extend(data.iter().map(|&s| f32::from_sample(s)));
            on_samples(&buf);
        },
        err_fn,
        None,
    )
}

/// Prints decoded frames until the stream dies or the user interrupts us.
fn report(
    rx: mpsc::Receiver<DecodedFrame>,
    level: &AtomicU32,
    json: bool,
    sample_rate: f64,
) {
    use std::io::Write;

    let mut last_frame: Option<Instant> = None;
    let mut previous: Option<DecodedFrame> = None;
    let mut dropped: u64 = 0;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(decoded) => {
                last_frame = Some(Instant::now());
                // A gap of more than one frame period between frame ends means
                // the decoder lost sync and skipped some.
                if let Some(prev) = previous {
                    let gap = decoded.end_sample.saturating_sub(prev.end_sample) as f64;
                    let period = sample_rate / decoded.measured_fps;
                    if gap > period * 1.5 {
                        dropped += (gap / period).round() as u64 - 1;
                    }
                }
                previous = Some(decoded);

                if json {
                    print_json(&decoded, dropped);
                } else {
                    print_live(&decoded, decibels(level), dropped);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let stale = last_frame.is_none_or(|t| t.elapsed() > SIGNAL_TIMEOUT);
                if stale && !json {
                    print!(
                        "\r  --:--:--:--   no LTC signal      {:>7.1} dBFS   \x1b[K",
                        decibels(level)
                    );
                    let _ = std::io::stdout().flush();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn print_live(decoded: &DecodedFrame, db: f64, dropped: u64) {
    use std::io::Write;
    let drops = if dropped > 0 {
        format!("   {dropped} dropped")
    } else {
        String::new()
    };
    print!(
        "\r  {}   {:>5} fps   ub {}   {:>7.1} dBFS{}   \x1b[K",
        decoded.frame,
        format_fps(decoded.nominal_fps()),
        decoded.frame.user_bits_hex(),
        db,
        drops
    );
    let _ = std::io::stdout().flush();
}

fn print_json(decoded: &DecodedFrame, dropped: u64) {
    let f = &decoded.frame;
    println!(
        r#"{{"timecode":"{}","hours":{},"minutes":{},"seconds":{},"frames":{},"drop_frame":{},"fps":{},"measured_fps":{:.4},"user_bits":"{}","end_sample":{},"dropped":{}}}"#,
        f,
        f.hours,
        f.minutes,
        f.seconds,
        f.frames,
        f.drop_frame,
        format_fps(decoded.nominal_fps()),
        decoded.measured_fps,
        f.user_bits_hex(),
        decoded.end_sample,
        dropped
    );
}

fn format_fps(fps: f64) -> String {
    if fps.fract() == 0.0 {
        format!("{fps:.0}")
    } else {
        format!("{fps:.2}")
    }
}

fn decibels(level: &AtomicU32) -> f64 {
    let amplitude = f32::from_bits(level.load(Ordering::Relaxed)) as f64;
    if amplitude <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * amplitude.log10()
    }
}

fn list_devices(host: &cpal::Host) -> Result<()> {
    let default = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());

    println!("input devices:");
    for device in host.input_devices()? {
        let id = device.id().map(|id| id.to_string()).unwrap_or_default();
        let marker = if Some(&id) == default.as_ref() {
            "*"
        } else {
            " "
        };
        let config = match device.default_input_config() {
            Ok(c) => format!("{} Hz, {} ch, {}", c.sample_rate(), c.channels(), c.sample_format()),
            Err(e) => format!("no input config: {e}"),
        };
        println!("{marker} {}\n    id: {id}\n    {config}", describe(&device));
    }
    println!("\n* = default. Pass an id or part of a name to --device.");
    Ok(())
}

fn find_device(host: &cpal::Host, want: &str) -> Result<Device> {
    if let Ok(id) = want.parse()
        && let Some(device) = host.device_by_id(&id)
    {
        return Ok(device);
    }

    let needle = want.to_lowercase();
    let mut matches: Vec<Device> = host
        .input_devices()?
        .filter(|d| {
            describe(d).to_lowercase().contains(&needle)
                || d.id().is_ok_and(|id| id.to_string().to_lowercase().contains(&needle))
        })
        .collect();

    match matches.len() {
        0 => Err(anyhow!(
            "no input device matches {want:?} — try --list-devices"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(anyhow!(
            "{want:?} matches several input devices: {}",
            matches
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn describe(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".to_string())
}
