//! Reading timecode off a Tentacle Sync E.
//!
//! [`ble`] reads it out of the device's Bluetooth advertisements, without
//! pairing or connecting to anything; [`ltc`] decodes it from an audio input;
//! [`freerun`] smooths either into a clock that ticks between readings. Both
//! sources decode into the one [`Timecode`], though they can't tell you quite
//! the same things about it — see the [`timecode`] module.
//!
//! [`ble::Scanner`] is the way in if you have a Tentacle and want the time off
//! it; its module docs have the two ways to read one.
//!
//! *shokushu* (触手) is Japanese for tentacle. This is an unofficial,
//! unaffiliated project: Tentacle Sync GmbH neither endorses nor supports it,
//! and the hardware is named here only to say what this reads.
//!
//! # What needs which feature
//!
//! The decoders have no dependencies and are always available: [`ble::parse`]
//! turns an advertisement's bytes into a reading, [`ltc::LtcDecoder`] turns
//! audio samples into frames, and [`freerun`] turns either into a clock. None
//! of them do any I/O, so none of them can fail.
//!
//! Getting hold of the bytes is what costs something, and that's what the
//! features gate:
//!
//! - `scan` brings in `btleplug` and a tokio runtime for [`ble::Scanner`].
//! - `audio` brings in `cpal`, for the `shokushu` binary. There's no library
//!   audio transport yet.
//! - `cli` is what the binaries need to be binaries.
//!
//! None are on by default, so a plain dependency is the decoders alone. Add
//! back whatever transport you need:
//!
//! ```toml
//! shokushu = { version = "0.1", features = ["scan"] }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod ble;
pub mod freerun;
pub mod ltc;
pub mod timecode;

/// Errors from anything that talks to hardware.
///
/// Gated on `scan` because that's the only thing here that can fail: the
/// decoders answer `None` for input they don't recognise rather than erroring,
/// since a stream of advertisements or of audio is expected to contain things
/// that aren't timecode.
#[cfg(feature = "scan")]
#[cfg_attr(docsrs, doc(cfg(feature = "scan")))]
pub mod error;

#[cfg(feature = "scan")]
pub use error::{Error, Result};
pub use timecode::{Rate, Timecode};
