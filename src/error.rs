//! What can go wrong.
//!
//! Only the adapter can, which is why this module is behind the `scan` feature
//! — see the crate docs.

/// An error from anything in this crate that talks to hardware.
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
