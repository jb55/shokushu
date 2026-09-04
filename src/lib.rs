//! Reading timecode off a Tentacle Sync E: [`ltc`] decodes it from an audio
//! input, [`ble`] reads it out of the device's Bluetooth advertisements and
//! [`freerun`] smooths those into a clock.

pub mod ble;
pub mod freerun;
pub mod ltc;
