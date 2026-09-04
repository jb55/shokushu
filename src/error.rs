//! What can go wrong.

/// An error from anything in this crate that talks to hardware.
///
/// The decoders can't fail this way — [`ble::parse`](crate::ble::parse) and
/// [`LtcDecoder`](crate::ltc::LtcDecoder) answer `None` or nothing at all for
/// input they don't recognise, since a stream of advertisements or audio is
/// expected to contain things that aren't timecode. Errors here are about the
/// adapter, not the bytes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The host has no Bluetooth adapter, or none the platform will hand over.
    #[error("no bluetooth adapter")]
    NoAdapter,

    #[error("bluetooth: {0}")]
    Bluetooth(#[from] btleplug::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
