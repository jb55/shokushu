//! Reading timecode off a Tentacle Sync E.
//!
//! [`ble`] reads it out of the device's Bluetooth advertisements, without
//! pairing or connecting to anything; [`ltc`] decodes it from an audio input;
//! [`freerun`] smooths either into a clock that ticks between readings. Both
//! sources decode into the one [`Timecode`], though they can't tell you quite
//! the same things about it — see the [`timecode`] module.
//!
//! [`ble`]'s `Scanner` is the way in if you have a Tentacle and want the time
//! off it; that module's docs have the two ways to read one.
//!
//! *shokushu* (触手) is Japanese for tentacle. This is an unofficial,
//! unaffiliated project: Tentacle Sync GmbH neither endorses nor supports it,
//! and the hardware is named here only to say what this reads.
//!
//! # Reading a Tentacle
//!
//! The whole path, with the `scan` feature on: find the devices, let the
//! advertisements land, and ask whichever one you care about what time it is.
//!
//! Every timecode advertisement anchors that device's [`freerun`] clock on the
//! way past, so `reading` answers at whatever rate you ask it. That's the point
//! of the arrangement — adverts arrive once or twice a second, and a display
//! driven straight off them lurches a dozen frames at a time.
//!
//! ```no_run
//! # #[cfg(feature = "scan")] {
//! use std::time::{Duration, Instant};
//!
//! use shokushu::ble::Scanner;
//! use shokushu::freerun::Reading;
//!
//! # async fn run() -> shokushu::Result<()> {
//! let mut scan = Scanner::start().await?;
//! let mut next_draw = Instant::now();
//!
//! loop {
//!     // Timecode out, on your schedule rather than the device's.
//!     let now = Instant::now();
//!     if now >= next_draw {
//!         next_draw = now + Duration::from_millis(40);
//!
//!         for device in scan.devices() {
//!             // `reading` needs `&mut`, so take it before borrowing the name.
//!             let reading = device.reading(now);
//!             let name = device.name().unwrap_or("<unnamed>");
//!             match reading {
//!                 Some(Reading::Running(tc)) => println!("{name}  {tc}"),
//!                 Some(Reading::Lost { last, since }) => {
//!                     println!("{name}  {last}  quiet for {:.1}s", since.as_secs_f64())
//!                 }
//!                 // In range but not sending timecode. Most of what a scan
//!                 // sees isn't a Tentacle, so this is the common case.
//!                 None => {}
//!             }
//!         }
//!     }
//!
//!     // Advertisements in. Each one anchors its own device's clock as it
//!     // arrives, whether or not anyone reads the event it produces.
//!     let Some(_event) = scan.next().await else { break };
//! }
//!
//! scan.stop().await
//! # }
//! # }
//! ```
//!
//! The scanner in [`ble`] keeps one clock per device, which is the arrangement
//! to copy: two boxes anchoring one clock don't average, they fight. Drive a
//! [`freerun::FreeRun`] yourself when the readings come from somewhere this
//! crate doesn't scan — your own Bluetooth stack, say, with [`ble::parse`]
//! turning the service data into a reading. `examples/scan.rs` is this loop
//! with the raw events shown too.
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
//! - `scan` brings in `btleplug` and a tokio runtime for [`ble`]'s `Scanner`.
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
