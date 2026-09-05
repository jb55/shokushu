//! Choosing an audio input and opening a stream on it.
//!
//! The thin layer over `cpal` that both audio-facing binaries want: pick a
//! device the way a `--device` flag means it, and get the samples as `f32`
//! whatever the hardware natively sends. It decodes nothing — [`ltc`] takes the
//! samples from here, or from wherever else you got them.
//!
//! [`ltc`]: crate::ltc
//!
//! The callback is handed `cpal`'s timing information alongside the samples,
//! which is the part that matters to a recorder. Knowing *when* the first
//! sample was captured — not when the callback was told about it — is what puts
//! a recording on a timecode timeline to better than a frame; see
//! [`InputCallbackInfo`](cpal::InputCallbackInfo) and what
//! `src/bin/shokushu-rec.rs` does with it.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, SupportedStreamConfig};

/// What can go wrong getting hold of an input.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The host has no default input, and none was named.
    #[error("no default input device")]
    NoDefaultInput,

    /// Nothing matched what was asked for.
    #[error("no input device matches {0:?}")]
    NoMatch(String),

    /// More than one device matched, so the choice would be arbitrary.
    #[error("{want:?} matches several input devices: {}", matches.join(", "))]
    Ambiguous { want: String, matches: Vec<String> },

    /// A sample format `cpal` supports and this doesn't convert from.
    #[error("unsupported sample format {0}")]
    UnsupportedFormat(SampleFormat),

    /// The host or the device refused something. `cpal` reports every one of
    /// these as the same type, so the context says which call it came from.
    #[error("{context}: {source}")]
    Cpal {
        context: &'static str,
        source: cpal::Error,
    },
}

impl Error {
    /// A `map_err` for the one `cpal` error type, naming what was being done.
    fn at(context: &'static str) -> impl Fn(cpal::Error) -> Error {
        move |source| Error::Cpal { context, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Opens an input stream, converting whatever the device sends to `f32`.
///
/// Samples reach the callback interleaved, in the device's channel order.
pub fn open_input_stream<D, E>(
    device: &Device,
    config: SupportedStreamConfig,
    on_samples: D,
    err_fn: E,
) -> Result<cpal::Stream>
where
    D: FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let stream_config = config.into();
    let stream = match config.sample_format() {
        SampleFormat::I8 => build::<i8, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::I16 => build::<i16, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::I32 => build::<i32, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::U8 => build::<u8, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::U16 => build::<u16, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::F32 => build::<f32, _, _>(device, stream_config, on_samples, err_fn),
        SampleFormat::F64 => build::<f64, _, _>(device, stream_config, on_samples, err_fn),
        other => return Err(Error::UnsupportedFormat(other)),
    }
    .map_err(Error::at("opening the input stream"))?;
    Ok(stream)
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
    D: FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let mut buf: Vec<f32> = Vec::new();
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            buf.clear();
            buf.extend(data.iter().map(|&s| f32::from_sample(s)));
            on_samples(&buf, info);
        },
        err_fn,
        None,
    )
}

/// The input `want` names — a device id, or part of a device name — or the
/// host's default when nothing is named.
pub fn open_device(host: &cpal::Host, want: Option<&str>) -> Result<Device> {
    match want {
        Some(want) => find_device(host, want),
        None => host.default_input_device().ok_or(Error::NoDefaultInput),
    }
}

/// The input device `want` names: a device id, or part of a device name.
///
/// Matching on part of a name is what makes a `--device external` usable, and
/// it is also how two devices can match one word — which is
/// [`Error::Ambiguous`] rather than a guess.
pub fn find_device(host: &cpal::Host, want: &str) -> Result<Device> {
    if let Ok(id) = want.parse()
        && let Some(device) = host.device_by_id(&id)
    {
        return Ok(device);
    }

    let needle = want.to_lowercase();
    let mut matches: Vec<Device> = host
        .input_devices()
        .map_err(Error::at("enumerating input devices"))?
        .filter(|d| {
            describe(d).to_lowercase().contains(&needle)
                || d.id()
                    .is_ok_and(|id| id.to_string().to_lowercase().contains(&needle))
        })
        .collect();

    match matches.len() {
        0 => Err(Error::NoMatch(want.to_string())),
        1 => Ok(matches.remove(0)),
        _ => Err(Error::Ambiguous {
            want: want.to_string(),
            matches: matches.iter().map(describe).collect(),
        }),
    }
}

/// Every input the host has, as (marker, description, id, config) lines ready
/// to print. The marker is `*` on the default input.
///
/// Here rather than in a binary because both of them offer the same
/// `--list-devices`, and because a device that won't report a config should
/// still appear in the list — with the reason where its config would be. A
/// device you cannot see is the hardest kind to pick.
pub fn describe_inputs(host: &cpal::Host) -> Result<Vec<(char, String, String, String)>> {
    let default = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());

    let mut lines = Vec::new();
    for device in host
        .input_devices()
        .map_err(Error::at("enumerating input devices"))?
    {
        let id = device.id().map(|id| id.to_string()).unwrap_or_default();
        let marker = if Some(&id) == default.as_ref() {
            '*'
        } else {
            ' '
        };
        let config = match device.default_input_config() {
            Ok(c) => format!(
                "{} Hz, {} ch, {}",
                c.sample_rate(),
                c.channels(),
                c.sample_format()
            ),
            Err(e) => format!("no input config: {e}"),
        };
        lines.push((marker, describe(&device), id, config));
    }
    Ok(lines)
}

/// Prints what [`describe_inputs`] found, in the form both binaries show it.
pub fn list_devices(host: &cpal::Host) -> Result<()> {
    println!("input devices:");
    for (marker, name, id, config) in describe_inputs(host)? {
        println!("{marker} {name}\n    id: {id}\n    {config}");
    }
    println!("\n* = default. Pass an id or part of a name to --device.");
    Ok(())
}

/// What to call a device in a message.
pub fn describe(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".to_string())
}
