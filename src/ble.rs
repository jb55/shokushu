//! Decoding the advertisements a Tentacle Sync E broadcasts over Bluetooth LE.
//!
//! The device advertises service data under the 16-bit UUID `0xFDAC`, in nine
//! byte packets that carry the running timecode. Nothing here comes from a
//! published spec — it's what the bytes did when watched against a device whose
//! timecode and date were known:
//!
//! ```text
//!   22 7d 19 0b 25 28 15   5f c6
//!   ~~ record type
//!      ~~ flags, meaning unknown
//!         ~~~~~~~~~~~~~~~~ data
//!                          ~~~~~ trailer
//! ```
//!
//! **Byte 1 is not a length**, though it read like one for a while. It was
//! `0x05` in every packet of the early captures — exactly the width of the data
//! field that follows — so this parser derived the field from it. Then two boxes
//! were connected to the Tentacle phone app to sync them, and byte 1 became
//! `0x7d` on both while the packets stayed nine bytes. Read as a length, 125
//! runs 118 bytes off the end, so every advertisement was rejected and the
//! scanner went silently blind.
//!
//! What it actually is, is unknown. Values seen: `0x05` and `0x07` before that
//! sync, `0x7c` and `0x7d` after — bits 3–6 turning on together and the bottom
//! bit or two flickering, which looks like flags and is not evidence of what
//! they flag. The layout here is therefore fixed rather than self-describing:
//! two header bytes, five data bytes, an optional two-byte trailer. That holds
//! across all 2,322 payloads captured either side of the change.
//!
//! Two record types turn up. `0x22` carries the timecode, as plain binary (not
//! BCD — seconds were seen reaching 0x3b and rolling to 0x00 as the minute
//! advanced), preceded by the frame rate:
//!
//! ```text
//!   22 7d | 19 0b 25 28 15        fps=25, 11:37:40:21
//! ```
//!
//! `0x42` carries the date, and this one *is* BCD:
//!
//! ```text
//!   42 7d | 00 26 09 04 02        2026-09-04
//! ```
//!
//! The two-byte trailer on a timecode record is a microsecond count of how far
//! into the current frame the reading was taken, big-endian. It isn't a
//! checksum: it matches no standard CRC-16 over the preceding bytes, and solving
//! over GF(2) rules out its being any linear function of them. What identifies
//! it is timing. Against host arrival times, `frames` alone tracks the wall
//! clock to within half a frame — a median error of 8 to 13 ms at 25 fps, the
//! quantisation you'd expect — and adding the trailer as microseconds tightens
//! that to 0.6 ms.
//!
//! Two things fix the scale at 1 MHz. Values span 39,000-odd of the 65,536 a
//! full-scale fraction would fill, never approaching the top of the range, and
//! 40,000 µs is exactly one frame at 25 fps; and free-fitting the scale against
//! arrival times lands between 39,376 and 40,500 across three independent
//! captures. Reading the bytes little-endian, or as a fraction over 65,536, both
//! fit worse — the latter by a factor of five to eight.
//!
//! Two caveats. Values run from about 3,700 to 43,600 rather than [0, 40000): a
//! fixed bias of a few milliseconds, whose origin is unknown, so this isn't
//! literally microseconds since the frame boundary. Nothing here corrects for
//! it, since it cancels in anything that compares two readings, which is all
//! this is used for. And the one-frame period is inferred from a 25 fps device,
//! the only rate yet observed; scaling by `fps` rather than hardcoding 40,000 is
//! reasoning, not evidence.
//!
//! On a date record the trailer means something else, and is ignored. Byte 7 is
//! always `a1`; byte 8 is `00` in 112 of 124 date records and something else in
//! the other 12, with no value repeating. Whatever that is, it is not a
//! microsecond count into a frame — a date record names no frame.
//!
//! Alongside the service data — in the same advertisement but a separate field,
//! and a separate event to a scanner — the device sends a five-byte
//! manufacturer record under company `0x043f`:
//!
//! ```text
//!   02 00 64 01 13
//!         ~~ battery percent
//! ```
//!
//! The low seven bits of byte 2 are the remaining charge and the top bit says
//! the device is charging:
//!
//! ```text
//!   0x60   0 1100000     96%, on battery
//!   0xe2   1 1100010     98%, charging
//! ```
//!
//! Two boxes advertising side by side read 100 and 97 while the other four bytes
//! were identical on both, and the one reading 97 dropped to 96 partway through
//! a capture, by itself. Then one of them was put on a charger three times over
//! an hour while the other stayed on battery as a control. Bit 7 went up on
//! plug-in and down on unplug every time, on the charging box and never on the
//! control, and the low seven bits climbed 96 → 97 → 98 → 99 → 100 in between,
//! one at a time. A field that tracks a cable on command, in both directions, is
//! not a coincidence.
//!
//! Masking matters: a charging device at 98% advertises `0xe2`, which is 226. A
//! reader that takes the byte whole reports a nonsense percentage, and one that
//! range-checks it against 100 — as this did at first — drops the reading
//! entirely and shows nothing exactly when a device is plugged in.
//!
//! That the scale is a *percentage* is one step less certain: 100 is the largest
//! value seen and 96 the smallest, so it's anchored at the top and unobserved
//! below. It reads as a percentage rather than, say, tenths of a volt, but only
//! a fuller discharge would show that.
//!
//! The other four bytes are unknown, with one hint. Byte 1 was `0x00` on both
//! boxes until the first charge, when it became `0x02` and stayed there through
//! every later unplug. It is not the charger, though: it changed 109 seconds
//! *after* the cable went in, by which point the battery had already gained a
//! percent, so it latches on something slower and never reset. `02` at byte 0 and
//! `01 13` at bytes 3-4 never moved at all.
//!
//! There is no Battery Service. The device's GATT server offers only Device
//! Information (`0x180a`) and its own `0xfdac`, with no `0x180f` and no battery
//! characteristic anywhere, so the advertisement is the only place a charge
//! level is published — which is just as well, since reading it needs no
//! connection.

use std::fmt;
use std::time::Duration;

use crate::timecode::{Rate, Timecode};

#[cfg(feature = "scan")]
#[cfg_attr(docsrs, doc(cfg(feature = "scan")))]
pub mod diagnostics;
#[cfg(feature = "scan")]
#[cfg_attr(docsrs, doc(cfg(feature = "scan")))]
pub mod scan;

#[cfg(feature = "scan")]
pub use scan::{Advertisement, Device, Event, Scanner};

/// The 16-bit service UUID the Tentacle advertises under.
pub const SERVICE_UUID_16: u16 = 0xFDAC;

const KIND_TIMECODE: u8 = 0x22;
const KIND_DATE: u8 = 0x42;

/// The record type and the flags byte, ahead of the data field.
///
/// Public because it's part of the wire format rather than an implementation
/// detail: anything describing a payload's shape — the scanner's diagnostics,
/// say — needs to agree with this parser about where the header ends.
pub const HEADER: usize = 2;

/// How wide the data field is. Fixed, in every record type and every packet
/// observed — including across a firmware or configuration change that moved
/// the flags byte. See the module docs for why this isn't read off the wire.
const DATA_LEN: usize = 5;

/// The date the Tentacle is set to, which it also writes into the LTC user bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advert {
    Timecode(Timecode),
    Date(Date),
}

/// Parses one `0xFDAC` service-data payload.
///
/// Returns `None` for anything that isn't a record type we recognise, or whose
/// fields are out of range — which is also the check that keeps us from reading
/// some other vendor's advertisement as timecode.
pub fn parse(data: &[u8]) -> Option<Advert> {
    let kind = *data.first()?;
    // Byte 1 is skipped, not read as a length — see the module docs. The data
    // field is at a fixed offset and a fixed width in every packet ever seen,
    // and deriving it from byte 1 is what blinded this parser once already.
    let body = data.get(HEADER..HEADER + DATA_LEN)?;

    match kind {
        KIND_TIMECODE => {
            let &[fps, hours, minutes, seconds, frames] = body else {
                return None;
            };
            let subframe_micros = match data.get(HEADER + DATA_LEN..) {
                Some(&[high, low, ..]) => u16::from_be_bytes([high, low]),
                _ => 0,
            };
            // Frames count 0..fps-1, so a frame at or past the rate means we've
            // misread the layout.
            if !(1..=120).contains(&fps)
                || hours > 23
                || minutes > 59
                || seconds > 59
                || frames >= fps
            {
                return None;
            }
            Some(Advert::Timecode(Timecode {
                hours,
                minutes,
                seconds,
                frames,
                // No drop-frame flag exists on the air; see the timecode module
                // docs for why that's "unknown" rather than "not drop-frame".
                rate: Rate::whole(fps),
                subframe: Duration::from_micros(subframe_micros as u64),
            }))
        }
        KIND_DATE => {
            let &[_, year, month, day, _] = body else {
                return None;
            };
            let (year, month, day) = (bcd(year)?, bcd(month)?, bcd(day)?);
            if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return None;
            }
            Some(Advert::Date(Date {
                year: 2000 + year as u16,
                month,
                day,
            }))
        }
        _ => None,
    }
}

/// One packed BCD byte as a number, or `None` if either nibble isn't a digit.
fn bcd(byte: u8) -> Option<u8> {
    let (high, low) = (byte >> 4, byte & 0x0f);
    (high <= 9 && low <= 9).then_some(high * 10 + low)
}

/// The Bluetooth SIG company identifier the Tentacle's manufacturer data is
/// advertised under.
pub const COMPANY_ID: u16 = 0x043F;

/// The bit of the battery byte that means "on a charger", leaving the charge
/// itself in the low seven.
const CHARGING: u8 = 0x80;

/// What the manufacturer record says about the device itself, as opposed to the
/// time it's keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Remaining charge, 0 to 100. See the module docs for how that scale was
    /// established, and for the bytes around it that weren't.
    pub battery_percent: u8,
    /// Whether the device is plugged in.
    pub charging: bool,
}

/// Parses one manufacturer-data record advertised under [`COMPANY_ID`].
///
/// Returns `None` for anything that isn't the five-byte record this device
/// sends, or whose battery byte is out of range once the charging bit is off it
/// — which is also what keeps another vendor's record from being read as a
/// charge level. The four bytes that aren't understood are deliberately not
/// required to hold any particular value: they were the same on both devices
/// ever seen, and rejecting a record for disagreeing with a sample of one
/// revision would be fitting to noise.
pub fn parse_manufacturer(data: &[u8]) -> Option<Status> {
    let &[_, _, battery, _, _] = data else {
        return None;
    };
    // The range check has to come after the mask, not before it: a charging
    // device advertises its percentage with the top bit set, so checking the
    // raw byte against 100 throws away every reading from a plugged-in box.
    let battery_percent = battery & !CHARGING;
    (battery_percent <= 100).then_some(Status {
        battery_percent,
        charging: battery & CHARGING != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payloads captured off a Tentacle Sync E running at 25 fps on 2026-09-04.
    const CAPTURED: &[(&[u8], &str)] = &[
        (&[0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14, 0x58, 0x62], "09:35:59:20"),
        (&[0x22, 0x05, 0x19, 0x09, 0x24, 0x00, 0x03, 0x58, 0xca], "09:36:00:03"),
        (&[0x22, 0x05, 0x19, 0x09, 0x22, 0x32, 0x03, 0x50, 0x56], "09:34:50:03"),
    ];

    /// Payloads captured off the same boxes after they were connected to the
    /// Tentacle phone app to sync them, which set byte 1 to `0x7d` and stopped
    /// the scanner decoding anything at all. Same nine-byte shape, same fields
    /// in the same places — one flag byte moved, and reading it as a length
    /// threw all of these away.
    const AFTER_APP_SYNC: &[(&[u8], &str)] = &[
        (&[0x22, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6], "11:37:40:21"),
        (&[0x22, 0x7d, 0x19, 0x0b, 0x1d, 0x0a, 0x10, 0x17, 0xc7], "11:29:10:16"),
        // Byte 1 is not even stable at 0x7d: four packets in a 500 s capture
        // came through as 0x7c, so a parser that special-cased the new value
        // would have thrown these away in turn.
        (&[0x22, 0x7c, 0x19, 0x0b, 0x1e, 0x08, 0x0e, 0x88, 0x11], "11:30:08:14"),
        // And one from before the sync where the bottom bit had flickered the
        // other way, at a byte 1 of 0x07.
        (&[0x22, 0x07, 0x19, 0x0b, 0x1c, 0x1e, 0x10, 0x44, 0x17], "11:28:30:16"),
    ];

    #[test]
    fn parses_captured_timecode() {
        for (payload, expected) in CAPTURED.iter().chain(AFTER_APP_SYNC) {
            let Some(Advert::Timecode(tc)) = parse(payload) else {
                panic!("{payload:02x?} did not parse as timecode");
            };
            assert_eq!(tc.to_string(), *expected);
            assert_eq!(tc.rate.fps, 25);
        }
    }

    #[test]
    fn byte_one_is_not_a_length() {
        // The regression. A nine-byte packet decodes the same whatever byte 1
        // says, because it says nothing about the layout: 0x05 was the old
        // value, 0x7d the one that arrived after a phone-app sync and blinded
        // the scanner, 0xff the reductio.
        for flags in [0x00, 0x05, 0x07, 0x7c, 0x7d, 0xff] {
            let payload = [0x22, flags, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];
            let Some(Advert::Timecode(tc)) = parse(&payload) else {
                panic!("byte 1 = {flags:#04x} was rejected");
            };
            assert_eq!(tc.to_string(), "11:37:40:21");
            assert_eq!(tc.subframe.as_micros(), 24518);
        }
    }

    #[test]
    fn the_range_checks_carry_the_rejecting_on_their_own() {
        // Nothing validates byte 1 any more, so the field checks are the only
        // thing keeping another vendor's 0xFDAC advertisement out of the
        // display. Same payload as above with one field pushed out of range,
        // for each field in turn.
        let good = [0x22u8, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];
        assert!(parse(&good).is_some());
        for (byte, bad) in [(2, 0x00), (3, 24), (4, 60), (5, 60), (6, 25)] {
            let mut payload = good;
            payload[byte] = bad;
            assert!(
                parse(&payload).is_none(),
                "byte {byte} = {bad:#04x} should have been rejected"
            );
        }
    }

    #[test]
    fn parses_the_captured_date() {
        // The date record moved its flags byte too, and its trailer's low byte
        // turns out not to be the constant it looked like — neither is allowed
        // to matter, since nothing here reads either.
        let captured: &[&[u8]] = &[
            &[0x42, 0x05, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0x00],
            &[0x42, 0x7d, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0x00],
            &[0x42, 0x7d, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0xf9],
            &[0x42, 0x7c, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0x47],
        ];
        for payload in captured {
            let Some(Advert::Date(date)) = parse(payload) else {
                panic!("{payload:02x?} did not parse as a date");
            };
            assert_eq!(date.to_string(), "2026-09-04");
        }
    }

    #[test]
    fn reads_the_subframe_position() {
        let payload = [0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14, 0x58, 0x62];
        let Some(Advert::Timecode(tc)) = parse(&payload) else {
            panic!("did not parse");
        };
        assert_eq!(tc.subframe.as_micros(), 22626);
        assert!((tc.subframe_seconds() - 0.022626).abs() < 1e-9);
        // 22626 µs of a 40 ms frame.
        assert!((tc.subframe_fraction() - 0.5657).abs() < 0.001);
    }

    #[test]
    fn a_missing_trailer_is_not_fatal() {
        // Only the declared data field is required; treat a short packet as
        // sitting on the frame boundary rather than throwing the reading away.
        let payload = [0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14];
        let Some(Advert::Timecode(tc)) = parse(&payload) else {
            panic!("did not parse");
        };
        assert_eq!(tc.subframe, Duration::ZERO);
        assert_eq!(tc.to_string(), "09:35:59:20");
    }

    #[test]
    fn seconds_are_binary_not_bcd() {
        // 0x3b is 59 as binary and nonsense as BCD; the captured rollover proves
        // it's the former, so this must parse rather than be rejected.
        let payload = [0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14, 0x00, 0x00];
        let Some(Advert::Timecode(tc)) = parse(&payload) else {
            panic!("rejected a real payload");
        };
        assert_eq!(tc.seconds, 59);
    }

    #[test]
    fn rejects_rubbish() {
        let cases: &[&[u8]] = &[
            &[],
            &[0x22],
            &[0x22, 0x05],
            &[0x22, 0x05, 0x19, 0x09, 0x23],             // data field truncated
            &[0x99, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14], // unknown record type
            &[0x22, 0x05, 0x19, 0x18, 0x23, 0x3b, 0x14], // hour 24
            &[0x22, 0x05, 0x19, 0x09, 0x3c, 0x3b, 0x14], // minute 60
            &[0x22, 0x05, 0x19, 0x09, 0x23, 0x3c, 0x14], // second 60
            &[0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x19], // frame 25 at 25 fps
            &[0x22, 0x05, 0x00, 0x09, 0x23, 0x3b, 0x00], // 0 fps
            &[0x42, 0x05, 0x00, 0x26, 0x1a, 0x04, 0x02], // month 0x1a isn't BCD
            &[0x42, 0x05, 0x00, 0x26, 0x13, 0x04, 0x02], // month 13
        ];
        for case in cases {
            assert!(parse(case).is_none(), "{case:02x?} should not have parsed");
        }
    }
    #[test]
    fn reads_the_battery_out_of_the_manufacturer_record() {
        // Both boxes as they advertised on 2026-09-04, and Liliana again after
        // she'd ticked down one. The four bytes around the battery were the same
        // on both devices and stayed put across the change.
        let cases = [
            ([0x02, 0x00, 0x64, 0x01, 0x13], 100),
            ([0x02, 0x00, 0x61, 0x01, 0x13], 97),
            ([0x02, 0x00, 0x60, 0x01, 0x13], 96),
        ];
        for (payload, expected) in cases {
            let Some(status) = parse_manufacturer(&payload) else {
                panic!("{payload:02x?} did not parse");
            };
            assert_eq!(status.battery_percent, expected);
            assert!(!status.charging, "{payload:02x?} is not on a charger");
        }
    }

    #[test]
    fn a_charging_device_reports_its_charge_and_not_a_number_over_100() {
        // Liliana on the cable, climbing, and then the moment it came out. The
        // first version of this rejected everything with the top bit set for
        // being over 100, which blanked the battery exactly when a box was
        // plugged in — so the mask has to happen before the range check.
        let cases = [
            ([0x02, 0x02, 0xe2, 0x01, 0x13], 98, true),
            ([0x02, 0x02, 0xe3, 0x01, 0x13], 99, true),
            ([0x02, 0x02, 0xe4, 0x01, 0x13], 100, true),
            ([0x02, 0x02, 0x64, 0x01, 0x13], 100, false),
        ];
        for (payload, percent, charging) in cases {
            let Some(status) = parse_manufacturer(&payload) else {
                panic!("{payload:02x?} did not parse");
            };
            assert_eq!(status.battery_percent, percent, "{payload:02x?}");
            assert_eq!(status.charging, charging, "{payload:02x?}");
        }
    }

    #[test]
    fn rejects_a_manufacturer_record_that_is_not_ours() {
        let cases: &[&[u8]] = &[
            &[],
            &[0x02, 0x00, 0x64, 0x01],             // four bytes, not five
            &[0x02, 0x00, 0x64, 0x01, 0x13, 0x00], // six
            &[0x02, 0x00, 0x65, 0x01, 0x13],       // 101%
            &[0x02, 0x00, 0xe5, 0x01, 0x13],       // 101% and charging
            &[0x02, 0x00, 0xff, 0x01, 0x13],       // 127% either way
        ];
        for case in cases {
            assert!(
                parse_manufacturer(case).is_none(),
                "{case:02x?} should not have parsed"
            );
        }
    }

    #[test]
    fn an_unknown_byte_changing_does_not_stop_the_battery_being_read() {
        // The four bytes either side aren't understood, so a device or a
        // firmware that sets them differently must still yield its charge
        // rather than being thrown away for disagreeing with our one sample.
        let Some(status) = parse_manufacturer(&[0x03, 0xff, 0x2a, 0x00, 0x7f]) else {
            panic!("rejected a record over bytes we don't claim to understand");
        };
        assert_eq!(status.battery_percent, 42);
        assert!(!status.charging);
    }
}
