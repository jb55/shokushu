//! Timecode, in the one form both sources decode into.
//!
//! The two ways to read a Tentacle disagree about what they can tell you, and
//! the disagreement is not symmetric — each fills a half of [`Rate`] the other
//! can't:
//!
//! - **Bluetooth carries the frame rate but no drop-frame flag.** 29.97 and 30
//!   are identical over the air, so a [`Rate`] built from an advertisement
//!   always has `drop_frame: false`, and that means *unknown*, not *known not to
//!   be drop-frame*. See [`ble`](crate::ble).
//! - **LTC carries the drop-frame flag but no rate at all.** Nothing in the 80
//!   bits names a frame rate; it's measured from the bit period the decoder
//!   locked to and snapped to the nearest rate anyone runs. So an [`LtcFrame`]'s
//!   rate is an inference, and its drop-frame flag is a reading.
//!
//! [`LtcFrame`]: crate::ltc::LtcFrame

use std::fmt;
use std::time::Duration;

/// A frame rate, and whether the numbering drops frames.
///
/// `fps` is the whole rate the numbering counts to — 30, not 29.97. The
/// fractional rates are `drop_frame` variants of the whole ones, which is what
/// [`Rate::exact_fps`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rate {
    pub fps: u8,
    pub drop_frame: bool,
}

impl Rate {
    /// A whole-number rate: 25, 30, and no dropped frame numbers.
    ///
    /// This is what an advertisement gives you, drop-frame flag included — which
    /// is to say, not included. See the module docs.
    pub fn whole(fps: u8) -> Rate {
        Rate {
            fps,
            drop_frame: false,
        }
    }

    pub fn new(fps: u8, drop_frame: bool) -> Rate {
        Rate { fps, drop_frame }
    }

    /// The rate frames actually go by, as opposed to the rate they're numbered
    /// at: 29.97 for 30 drop-frame, and the nominal rate otherwise.
    ///
    /// Drop-frame numbering exists to reconcile the two — NTSC runs 1000/1001 of
    /// the nominal rate, so numbering every frame would drift about 3.6 seconds
    /// off wall clock per hour, and dropping 108 numbers an hour takes it back
    /// out.
    pub fn exact_fps(&self) -> f64 {
        match self.drop_frame {
            true => self.fps as f64 * 1000.0 / 1001.0,
            false => self.fps as f64,
        }
    }

    /// How many frames a 24-hour day holds at this rate, which is where timecode
    /// wraps round to zero.
    ///
    /// A zero rate is floored at one. It can't come out of either decoder, but
    /// [`Rate`] is a public struct with public fields, so keep the division
    /// defined.
    pub fn frames_per_day(&self) -> f64 {
        24.0 * 3600.0 * self.fps.max(1) as f64
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.drop_frame {
            // Two decimal places is the conventional spelling, and exact_fps is
            // never a round number here.
            true => write!(f, "{:.2} fps", self.exact_fps()),
            false => write!(f, "{} fps", self.fps),
        }
    }
}

/// A timecode reading, from either source.
///
/// The named fields are the frame this reading falls in; [`Timecode::subframe`]
/// is how far into that frame it was actually taken, which is what lets a
/// reading be placed on a timeline finer than the frame it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub rate: Rate,
    /// How far into the frame this reading was taken.
    ///
    /// Zero from LTC, which is frame-aligned by construction — an LTC frame *is*
    /// the interval it names, so there's no offset to report and
    /// [`DecodedFrame::end_sample`] is where its timing lives instead.
    ///
    /// From Bluetooth this is the advertisement's sub-frame field, whose exact
    /// microseconds `as_micros` hands back. Mind the caveats in the
    /// [`ble`](crate::ble) module docs: there's a fixed bias of a few
    /// milliseconds in it, so it can exceed one frame, and it isn't literally
    /// microseconds since the frame boundary.
    ///
    /// [`DecodedFrame::end_sample`]: crate::ltc::DecodedFrame::end_sample
    pub subframe: Duration,
}

impl Timecode {
    /// A frame-aligned timecode — no sub-frame offset.
    pub fn new(hours: u8, minutes: u8, seconds: u8, frames: u8, rate: Rate) -> Timecode {
        Timecode {
            hours,
            minutes,
            seconds,
            frames,
            rate,
            subframe: Duration::ZERO,
        }
    }

    /// How far into the current frame this reading was taken, in seconds.
    ///
    /// Advertisements arrive only a couple of times a second, so this is what
    /// makes it possible to place a reading on a timeline to better than a
    /// millisecond rather than to the frame it names.
    pub fn subframe_seconds(&self) -> f64 {
        self.subframe.as_secs_f64()
    }

    /// The same, as a fraction of a frame.
    ///
    /// Usually 0.0 to just under 1.0, but a raw Bluetooth reading can exceed 1.0
    /// by the fixed bias the [`ble`](crate::ble) module docs describe.
    /// Interpolated timecode, which is built from a position rather than
    /// received, always sits inside a frame.
    pub fn subframe_fraction(&self) -> f64 {
        self.subframe_seconds() * self.rate.fps as f64
    }

    /// Where this reading sits on a timeline of frames since midnight, the
    /// sub-frame fraction included.
    ///
    /// This is the form to do arithmetic in: extrapolating a reading forward, or
    /// comparing two of them, is addition here and a mess of carries otherwise.
    ///
    /// # Drop-frame
    ///
    /// Wrong, and deliberately so, for a drop-frame rate. Drop-frame skips frame
    /// *numbers* — 00 and 01 at the top of every minute except every tenth — so
    /// the count of frames since midnight isn't the linear function of the
    /// fields that this computes. Nothing in the crate reaches it: the only
    /// caller is [`freerun`](crate::freerun), which is fed by Bluetooth, which
    /// never sets the flag. Rather than ship arithmetic that's quietly off by up
    /// to a couple of seconds a day, this debug-asserts; use
    /// [`Timecode::checked_frame_position`] where the rate isn't known in
    /// advance.
    pub fn frame_position(&self) -> f64 {
        debug_assert!(
            !self.rate.drop_frame,
            "frame_position does not implement drop-frame numbering"
        );
        self.linear_position()
    }

    /// [`Timecode::frame_position`], or `None` at a drop-frame rate where it has
    /// no answer to give.
    pub fn checked_frame_position(&self) -> Option<f64> {
        match self.rate.drop_frame {
            true => None,
            false => Some(self.linear_position()),
        }
    }

    fn linear_position(&self) -> f64 {
        let whole = ((self.hours as u64 * 60 + self.minutes as u64) * 60 + self.seconds as u64)
            * self.rate.fps as u64
            + self.frames as u64;
        whole as f64 + self.subframe_fraction()
    }

    /// The timecode at a position on that timeline, wrapping at 24 hours.
    ///
    /// The fractional part of `position` becomes the sub-frame field, so this
    /// round-trips [`Timecode::frame_position`] to within a microsecond.
    ///
    /// Carries the same drop-frame caveat, for the same reason.
    pub fn at_frame_position(position: f64, rate: Rate) -> Timecode {
        debug_assert!(
            !rate.drop_frame,
            "at_frame_position does not implement drop-frame numbering"
        );
        let position = position.rem_euclid(rate.frames_per_day());
        let whole = position.floor();
        let fps = rate.fps.max(1) as u64;
        let mut count = whole as u64;
        let frames = (count % fps) as u8;
        count /= fps;
        let seconds = (count % 60) as u8;
        count /= 60;
        let minutes = (count % 60) as u8;
        let hours = (count / 60 % 24) as u8;
        Timecode {
            hours,
            minutes,
            seconds,
            frames,
            rate,
            // `position - whole` is in [0, 1) and the rate is at least one, so
            // this is finite and non-negative — the two things from_secs_f64
            // panics on.
            subframe: Duration::from_secs_f64((position - whole) / rate.fps.max(1) as f64),
        }
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Drop-frame timecode is conventionally written with a semicolon.
        let sep = if self.rate.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(fps: u8, h: u8, m: u8, s: u8, f: u8, micros: u64) -> Timecode {
        Timecode {
            hours: h,
            minutes: m,
            seconds: s,
            frames: f,
            rate: Rate::whole(fps),
            subframe: Duration::from_micros(micros),
        }
    }

    #[test]
    fn frame_positions_round_trip() {
        // Every field has to survive the trip through a single f64, sub-frame
        // microseconds included, since that position is what gets extrapolated.
        let cases = [
            (25, 0, 0, 0, 0, 0u64),
            (25, 9, 35, 59, 20, 22626),
            (30, 23, 59, 59, 29, 33_000),
            (24, 12, 0, 0, 12, 41_666),
        ];
        for (fps, hours, minutes, seconds, frames, micros) in cases {
            let tc = at(fps, hours, minutes, seconds, frames, micros);
            let back = Timecode::at_frame_position(tc.frame_position(), tc.rate);
            assert_eq!(back.to_string(), tc.to_string());
            let got = back.subframe.as_micros() as u64;
            assert!(
                got.abs_diff(micros) <= 1,
                "{tc}: {micros} µs became {got} µs"
            );
        }
    }

    #[test]
    fn a_biased_subframe_still_round_trips_as_a_position() {
        // Sub-frame values run a few milliseconds past a frame period — see the
        // ble module docs — so a raw reading can sit outside the frame it names.
        // The position is what has to survive; the frame number carries.
        let tc = at(25, 9, 35, 59, 20, 43_581);
        assert!(tc.subframe_fraction() > 1.0);
        let position = tc.frame_position();
        let back = Timecode::at_frame_position(position, tc.rate);
        assert_eq!(back.to_string(), "09:35:59:21");
        assert!((back.frame_position() - position).abs() < 1e-3);
    }

    #[test]
    fn a_frame_position_wraps_at_midnight() {
        // A free-running clock counts past the end of the day; the display has
        // to come back round to zero rather than showing hour 24.
        let rate = Rate::whole(25);
        let tc = Timecode::at_frame_position(rate.frames_per_day() + 3.5, rate);
        assert_eq!(tc.to_string(), "00:00:00:03");
        assert!((tc.subframe_fraction() - 0.5).abs() < 0.001);
        // And a position from before it, which is what a reading arriving late
        // across the boundary looks like once unwrapped.
        assert_eq!(
            Timecode::at_frame_position(-1.0, rate).to_string(),
            "23:59:59:24"
        );
    }

    #[test]
    fn drop_frame_is_written_with_a_semicolon() {
        let ndf = Timecode::new(1, 2, 3, 4, Rate::whole(30));
        let df = Timecode::new(1, 2, 3, 4, Rate::new(30, true));
        assert_eq!(ndf.to_string(), "01:02:03:04");
        assert_eq!(df.to_string(), "01:02:03;04");
    }

    #[test]
    fn drop_frame_declines_to_do_arithmetic() {
        // The alternative to declining is arithmetic that's wrong by up to a
        // couple of seconds a day without saying so.
        let df = Timecode::new(1, 2, 3, 4, Rate::new(30, true));
        assert_eq!(df.checked_frame_position(), None);
        assert!(Timecode::new(1, 2, 3, 4, Rate::whole(30))
            .checked_frame_position()
            .is_some());
    }

    #[test]
    fn exact_fps_is_the_ntsc_rate_when_dropping() {
        assert_eq!(Rate::whole(30).exact_fps(), 30.0);
        let df = Rate::new(30, true).exact_fps();
        assert!((df - 29.97).abs() < 0.01, "{df}");
        assert_eq!(Rate::new(30, true).to_string(), "29.97 fps");
        assert_eq!(Rate::whole(25).to_string(), "25 fps");
    }

    #[test]
    fn a_zero_rate_stays_defined() {
        // Neither decoder can produce one, but the fields are public.
        let rate = Rate::whole(0);
        assert_eq!(rate.frames_per_day(), 24.0 * 3600.0);
        let _ = Timecode::at_frame_position(1.0, rate);
    }
}
