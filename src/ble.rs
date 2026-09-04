//! Decoding the advertisements a Tentacle Sync E broadcasts over Bluetooth LE.
//!
//! The device advertises service data under the 16-bit UUID `0xFDAC`, in nine
//! byte packets that carry the running timecode. Nothing here comes from a
//! published spec — it's what the bytes did when watched against a device whose
//! timecode and date were known:
//!
//! ```text
//!   22 05 19 09 23 3b 14   58 62
//!   ~~ record type
//!      ~~ length of the data field
//!         ~~~~~~~~~~~~~~~~ data
//!                          ~~~~~ trailer
//! ```
//!
//! Two record types turn up. `0x22` carries the timecode, as plain binary (not
//! BCD — seconds were seen reaching 0x3b and rolling to 0x00 as the minute
//! advanced), preceded by the frame rate:
//!
//! ```text
//!   22 05 | 19 09 23 3b 14        fps=25, 09:35:59:20
//! ```
//!
//! `0x42` carries the date, and this one *is* BCD:
//!
//! ```text
//!   42 05 | 00 26 09 04 02        2026-09-04
//! ```
//!
//! The two-byte trailer on a timecode record is the position *within* the
//! current frame, big-endian over a full scale of 65536. It isn't a checksum:
//! it matches no standard CRC-16 over the preceding bytes, and solving over
//! GF(2) rules out its being any linear function of them. What identifies it is
//! timing. Against host arrival times, `frames` alone tracks the wall clock to
//! within half a frame — 23 ms at 25 fps, exactly the quantisation you'd expect
//! — while adding the trailer as a fraction of a frame tightens that to 10 ms,
//! about what Bluetooth delivery jitter accounts for. Reading the same two bytes
//! little-endian makes the fit worse than not using them at all, which is what
//! rules out a coincidence.
//!
//! On a date record the trailer is instead a fixed `a1 00`, so it means
//! something else there, and is ignored.

use std::fmt;

/// The 16-bit service UUID the Tentacle advertises under.
pub const SERVICE_UUID_16: u16 = 0xFDAC;

const KIND_TIMECODE: u8 = 0x22;
const KIND_DATE: u8 = 0x42;

/// Timecode as the Tentacle broadcasts it.
///
/// There's no drop-frame flag in here, unlike an LTC frame: only the whole frame
/// rate is transmitted, so 29.97 and 30 look alike over the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    pub fps: u8,
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    /// How far into the current frame this reading was taken, over a full scale
    /// of 65536. See [`Timecode::subframe_fraction`].
    pub subframe: u16,
}

impl Timecode {
    /// How far into the current frame this reading was taken, from 0.0 at the
    /// frame boundary to just under 1.0.
    ///
    /// Advertisements arrive only a couple of times a second, so this is what
    /// makes it possible to place a reading on a timeline more precisely than
    /// the frame it names.
    pub fn subframe_fraction(&self) -> f64 {
        self.subframe as f64 / 65536.0
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

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
    let length = *data.get(1)? as usize;
    let body = data.get(2..2 + length)?;

    match kind {
        KIND_TIMECODE => {
            let &[fps, hours, minutes, seconds, frames] = body else {
                return None;
            };
            let subframe = match data.get(2 + length..) {
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
                fps,
                hours,
                minutes,
                seconds,
                frames,
                subframe,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Payloads captured off a Tentacle Sync E running at 25 fps on 2026-09-04.
    const CAPTURED: &[(&[u8], &str)] = &[
        (&[0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14, 0x58, 0x62], "09:35:59:20"),
        (&[0x22, 0x05, 0x19, 0x09, 0x24, 0x00, 0x03, 0x58, 0xca], "09:36:00:03"),
        (&[0x22, 0x05, 0x19, 0x09, 0x22, 0x32, 0x03, 0x50, 0x56], "09:34:50:03"),
    ];

    #[test]
    fn parses_captured_timecode() {
        for (payload, expected) in CAPTURED {
            let Some(Advert::Timecode(tc)) = parse(payload) else {
                panic!("{payload:02x?} did not parse as timecode");
            };
            assert_eq!(tc.to_string(), *expected);
            assert_eq!(tc.fps, 25);
        }
    }

    #[test]
    fn parses_the_captured_date() {
        let payload = [0x42, 0x05, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0x00];
        let Some(Advert::Date(date)) = parse(&payload) else {
            panic!("did not parse as a date");
        };
        assert_eq!(date.to_string(), "2026-09-04");
    }

    #[test]
    fn reads_the_subframe_position() {
        let payload = [0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14, 0x58, 0x62];
        let Some(Advert::Timecode(tc)) = parse(&payload) else {
            panic!("did not parse");
        };
        assert_eq!(tc.subframe, 0x5862);
        assert!((tc.subframe_fraction() - 0.3452).abs() < 0.001);
    }

    #[test]
    fn a_missing_trailer_is_not_fatal() {
        // Only the declared data field is required; treat a short packet as
        // sitting on the frame boundary rather than throwing the reading away.
        let payload = [0x22, 0x05, 0x19, 0x09, 0x23, 0x3b, 0x14];
        let Some(Advert::Timecode(tc)) = parse(&payload) else {
            panic!("did not parse");
        };
        assert_eq!(tc.subframe, 0);
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
}
