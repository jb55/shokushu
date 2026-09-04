//! Reading timecode off a Tentacle Sync E.
//!
//! [`ble`] reads it out of the device's Bluetooth advertisements, without
//! pairing or connecting to anything; [`ltc`] decodes it from an audio input;
//! [`freerun`] smooths either into a clock that ticks between readings.
//!
//! Both sources decode into the one [`Timecode`], though they can't tell you
//! quite the same things about it — see the [`timecode`] module.
//!
//! ```no_run
//! use tentacle::ble::{Event, Scanner};
//!
//! # async fn run() -> tentacle::Result<()> {
//! let mut scan = Scanner::builder().name("ricki").start().await?;
//! while let Some(event) = scan.next().await {
//!     if let Event::Timecode { timecode, .. } = event {
//!         println!("{timecode}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Advertisements arrive only once or twice a second, so anything drawing at
//! its own refresh rate should read the clocks instead:
//!
//! ```no_run
//! # use std::time::Instant;
//! # use tentacle::ble::Scanner;
//! # async fn run(scan: &mut Scanner) {
//! for device in scan.devices() {
//!     if let Some(reading) = device.reading(Instant::now()) {
//!         println!("{:?} {reading:?}", device.name());
//!     }
//! }
//! # }
//! ```

pub mod ble;
pub mod error;
pub mod freerun;
pub mod ltc;
pub mod timecode;

pub use error::{Error, Result};
pub use timecode::{Rate, Timecode};
