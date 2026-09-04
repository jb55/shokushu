//! Reading timecode off a Tentacle Sync E: [`ltc`] decodes it from an audio
//! input, [`ble`] reads it out of the device's Bluetooth advertisements.

pub mod ble;
pub mod ltc;
