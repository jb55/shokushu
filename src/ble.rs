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
    /// How far into the current frame this reading was taken, in microseconds.
    /// See [`Timecode::subframe_seconds`], and the caveats in the module docs:
    /// there's a fixed bias of a few milliseconds in here.
    pub subframe_micros: u16,
}

impl Timecode {
    /// How far into the current frame this reading was taken, in seconds.
    ///
    /// Advertisements arrive only a couple of times a second, so this is what
    /// makes it possible to place a reading on a timeline to better than a
    /// millisecond rather than to the frame it names.
    pub fn subframe_seconds(&self) -> f64 {
        self.subframe_micros as f64 / 1e6
    }

    /// The same, as a fraction of a frame.
    ///
    /// Usually 0.0 to just under 1.0, but a raw reading can exceed 1.0 by the
    /// fixed bias the module docs describe. Interpolated timecode, which is
    /// built from a position rather than received, always sits inside a frame.
    pub fn subframe_fraction(&self) -> f64 {
        self.subframe_seconds() * self.fps as f64
    }

    /// Where this reading sits on a timeline of frames since midnight, the
    /// sub-frame fraction included.
    ///
    /// This is the form to do arithmetic in: extrapolating a reading forward, or
    /// comparing two of them, is addition here and a mess of carries otherwise.
    pub fn frame_position(&self) -> f64 {
        let whole = ((self.hours as u64 * 60 + self.minutes as u64) * 60 + self.seconds as u64)
            * self.fps as u64
            + self.frames as u64;
        whole as f64 + self.subframe_fraction()
    }

    /// The timecode at a position on that timeline, wrapping at 24 hours.
    ///
    /// The fractional part of `position` becomes the sub-frame field, so this
    /// round-trips [`Timecode::frame_position`] to within a microsecond.
    pub fn at_frame_position(position: f64, fps: u8) -> Timecode {
        let position = position.rem_euclid(frames_per_day(fps));
        let whole = position.floor();
        // A zero rate can't come out of [`parse`], but this is a public
        // constructor, so keep the division defined.
        let rate = fps.max(1) as u64;
        let mut count = whole as u64;
        let frames = (count % rate) as u8;
        count /= rate;
        let seconds = (count % 60) as u8;
        count /= 60;
        let minutes = (count % 60) as u8;
        let hours = (count / 60 % 24) as u8;
        Timecode {
            fps,
            hours,
            minutes,
            seconds,
            frames,
            // Saturating, so a fraction of exactly 1.0 can't wrap to 0.
            subframe_micros: ((position - whole) * 1e6 / fps.max(1) as f64) as u16,
        }
    }
}

/// How many frames a 24-hour day holds at this rate, which is where timecode
/// wraps round to zero. A zero rate is floored at one, as above.
pub fn frames_per_day(fps: u8) -> f64 {
    24.0 * 3600.0 * fps.max(1) as f64
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
            let subframe_micros = match data.get(2 + length..) {
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
                subframe_micros,
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
        assert_eq!(tc.subframe_micros, 22626);
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
        assert_eq!(tc.subframe_micros, 0);
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
    fn frame_positions_round_trip() {
        // Every field has to survive the trip through a single f64, sub-frame
        // microseconds included, since that position is what gets extrapolated.
        let cases = [
            (25, 0, 0, 0, 0, 0u16),
            (25, 9, 35, 59, 20, 22626),
            (30, 23, 59, 59, 29, 33_000),
            (24, 12, 0, 0, 12, 41_666),
        ];
        for (fps, hours, minutes, seconds, frames, subframe_micros) in cases {
            let tc = Timecode { fps, hours, minutes, seconds, frames, subframe_micros };
            let back = Timecode::at_frame_position(tc.frame_position(), fps);
            assert_eq!(back.to_string(), tc.to_string());
            assert!(
                back.subframe_micros.abs_diff(subframe_micros) <= 1,
                "{tc}: {subframe_micros} µs became {} µs",
                back.subframe_micros
            );
        }
    }

    #[test]
    fn a_biased_subframe_still_round_trips_as_a_position() {
        // Sub-frame values run a few milliseconds past a frame period — see the
        // module docs — so a raw reading can sit outside the frame it names. The
        // position is what has to survive; the frame number carries.
        let tc = Timecode {
            fps: 25,
            hours: 9,
            minutes: 35,
            seconds: 59,
            frames: 20,
            subframe_micros: 43_581,
        };
        assert!(tc.subframe_fraction() > 1.0);
        let position = tc.frame_position();
        let back = Timecode::at_frame_position(position, 25);
        assert_eq!(back.to_string(), "09:35:59:21");
        assert!((back.frame_position() - position).abs() < 1e-3);
    }

    #[test]
    fn a_frame_position_wraps_at_midnight() {
        // A free-running clock counts past the end of the day; the display has
        // to come back round to zero rather than showing hour 24.
        let midnight = frames_per_day(25);
        let tc = Timecode::at_frame_position(midnight + 3.5, 25);
        assert_eq!(tc.to_string(), "00:00:00:03");
        assert!((tc.subframe_fraction() - 0.5).abs() < 0.001);
        // And a position from before it, which is what a reading arriving late
        // across the boundary looks like once unwrapped.
        assert_eq!(
            Timecode::at_frame_position(-1.0, 25).to_string(),
            "23:59:59:24"
        );
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
