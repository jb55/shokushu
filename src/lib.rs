//! Reading timecode off a Tentacle Sync E: [`ltc`] decodes it from an audio
//! input, [`ble`] reads it out of the device's Bluetooth advertisements and
//! [`freerun`] smooths those into a clock.
//!
//! Both sources decode into the one [`Timecode`], though they can't tell you
//! quite the same things about it — see the [`timecode`] module.

pub mod ble;
pub mod freerun;
pub mod ltc;
pub mod timecode;

pub use timecode::{Rate, Timecode};
