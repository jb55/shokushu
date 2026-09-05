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

    /// A scan ran its course without a Tentacle answering.
    ///
    /// Only raised by something that needs a *particular* box — the scanner
    /// itself never raises it, since "nothing in range yet" is an ordinary
    /// state for a scan rather than a failure of one. See
    /// [`ble::jam`](crate::ble::jam).
    #[error("no Tentacle advertising 0x{:04X}{}", crate::ble::SERVICE_UUID_16,
            match filter { Some(f) => format!(" named like {f:?}"), None => String::new() })]
    NoTentacle { filter: Option<String> },

    /// The box would not accept a connection.
    ///
    /// It stops accepting them after a few dozen rapid ones while still
    /// advertising, so this and a healthy-looking scan are not a contradiction.
    /// Leave it alone for a while.
    #[error("could not connect to {name}: {reason}")]
    Connect { name: String, reason: String },

    /// The box connected and its GATT tree had no `0dab144c` in it.
    ///
    /// Every Tentacle seen so far offers it, so this is a box that is not one,
    /// or a firmware that has moved the timecode somewhere else.
    #[error("no timecode characteristic on this device")]
    NoTimecodeCharacteristic,

    /// A capture was taken and could not be reduced.
    ///
    /// Here so that collecting samples and reducing them can share one error
    /// type, since the collecting half needs hardware and the reducing half
    /// does not: [`jam::Flaw`](crate::jam::Flaw) is ungated and is its own
    /// error, and code that only reduces should keep using it directly rather
    /// than reaching for this.
    #[error("{0}")]
    Calibration(#[from] crate::jam::Flaw),

    #[error("bluetooth: {0}")]
    Bluetooth(#[from] btleplug::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
