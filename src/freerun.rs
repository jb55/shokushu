//! Turning sparse timecode readings into something that looks like a clock.
//!
//! Advertisements arrive only one or two times a second, so a display redrawn
//! when a packet lands visibly jumps. [`FreeRun`] keeps a local clock instead:
//! each advertisement anchors it, and between anchors the position is
//! extrapolated from the host's monotonic clock, so the display can tick at the
//! frame rate.
//!
//! This is cosmetic. Interpolation makes the display smooth, not more accurate:
//! it adds no information the advertisements didn't carry, and it can't tell you
//! anything the anchors either side of it didn't.
//!
//! # Using it
//!
//! Two calls, driven by two different clocks — which is the whole point, and
//! the thing to get right:
//!
//! - [`FreeRun::anchor`] every time a reading arrives, paired with the host
//!   time it arrived at. You don't control when this happens.
//! - [`FreeRun::sample`] whenever you want to know what to show. You do control
//!   when this happens, and it needn't have anything to do with the first.
//!
//! ```
//! use std::time::{Duration, Instant};
//!
//! use shokushu::freerun::{FreeRun, Reading};
//! use shokushu::{Rate, Timecode};
//!
//! let mut clock = FreeRun::default();
//!
//! // Nothing has arrived yet, so there is nothing to show. Not an error — a
//! // scan sees plenty of devices that never send timecode at all.
//! assert!(clock.sample(Instant::now()).is_none());
//!
//! // One advertisement lands: 10:00:00:00 at 25 fps.
//! let arrived = Instant::now();
//! clock.anchor(&Timecode::new(10, 0, 0, 0, Rate::whole(25)), arrived);
//!
//! // Half a second on, the clock has walked twelve frames by itself. No second
//! // advertisement was needed, which is the difference between a display that
//! // ticks and one that lurches twice a second.
//! let Some(Reading::Running(tc)) = clock.sample(arrived + Duration::from_millis(500)) else {
//!     panic!("anchored, so it runs");
//! };
//! assert_eq!((tc.hours, tc.minutes, tc.seconds, tc.frames), (10, 0, 0, 12));
//!
//! // Nothing more arrives. Past `HOLDOVER` it stops rather than inventing
//! // timecode, and hands back the last reading it actually got.
//! let Some(Reading::Lost { last, since }) = clock.sample(arrived + Duration::from_secs(6)) else {
//!     panic!("past HOLDOVER");
//! };
//! assert_eq!(last, Timecode::new(10, 0, 0, 0, Rate::whole(25)));
//! assert!(since >= Duration::from_secs(6));
//! ```
//!
//! The scanner in [`ble`](crate::ble) does the anchoring for you — each
//! `Device` there owns one of these, and `Device::reading` is its `sample`.
//! Reach for `FreeRun` directly when the readings come from somewhere else, or
//! when you're driving the scan yourself.
//!
//! ## Getting it right
//!
//! **One clock per device.** Anchoring one `FreeRun` from two boxes doesn't
//! average them, it fights: each anchor drags the model onto a different
//! timeline, and two boxes more than half a second apart make every anchor a
//! snap to whichever spoke last. Two at different frame rates is worse still —
//! a rate change throws the model away and starts again. Key them by device,
//! the way the [`ble`](crate::ble) scanner does.
//!
//! **`at` is when the packet arrived, not when you got round to it.** The whole
//! scheme rests on pairing a reading with the host time it represents, so stamp
//! [`Instant::now`] the moment the packet comes off the wire — before any
//! `await`, lookup or lock that could sit between the two. An anchor is only as
//! good as its timestamp.
//!
//! **Sample at your refresh rate.** That's what this exists for. Sampling
//! faster than the frame rate isn't wrong, it just returns the same frame
//! number with a finer sub-frame offset, which nothing displaying frames can
//! show. Sampling much slower than the frame rate means dropping frames you
//! could have drawn.
//!
//! **Non-decreasing `now`.** [`FreeRun::sample`] never hands out a position
//! below the last one it gave — that's what keeps the slew loop from showing as
//! a display ticking backwards. A `now` earlier than the previous call is
//! clamped to that previous reading rather than rewinding, so sampling from two
//! places without ordering them gets you a stalled clock, not a wrong one.
//!
//! **Non-drop-frame rates only.** [`Timecode::frame_position`], which all of
//! this is arithmetic on, doesn't implement drop-frame numbering and
//! debug-asserts on it. Bluetooth never sets the flag so nothing in this crate
//! reaches it, but a reading from elsewhere can: check
//! [`Rate::drop_frame`](crate::Rate#structfield.drop_frame) before anchoring
//! one.
//!
//! # How it works
//!
//! Three things make it more than `anchor + elapsed`:
//!
//! - **Jitter.** An advertisement's sub-frame field places it to well under a
//!   millisecond, but not exactly, so snapping to each new one nudges the clock
//!   back and forth — and a timecode display that ticks backwards reads as
//!   broken however small the step. Each anchor's error is folded in a fraction
//!   at a time instead, which averages the jitter down, and the position handed
//!   out never decreases.
//! - **Drift.** Host and Tentacle keep time on separate crystals, so
//!   extrapolating at exactly the nominal frame rate walks off over a long run.
//!   The real rate is measured from anchors over a long baseline, bounded to the
//!   few hundred ppm two crystals can plausibly disagree by.
//! - **Signal loss.** Extrapolation stops after [`HOLDOVER`] without an
//!   advertisement, and says so, rather than confidently inventing timecode.

use std::time::{Duration, Instant};

use crate::timecode::{Rate, Timecode};

/// How much of a new anchor's error is taken out on arrival. The remainder is
/// left to the anchors after it, so jitter averages out instead of being chased.
///
/// Anchors are good to well under a millisecond, so there's little to damp:
/// half of each error leaves a residual far inside a frame while still
/// converging in two or three adverts after anything disturbs the clock.
const SLEW_GAIN: f64 = 0.5;

/// An error past this is something real — the timecode being changed on the
/// device, or a jam-sync — rather than jitter, so snap to it instead of spending
/// a minute slewing there.
const SNAP: Duration = Duration::from_millis(500);

/// How long to keep extrapolating with nothing arriving.
///
/// Reception is burstier than the nominal one-or-two adverts a second suggests:
/// over 40 s with the device on the desk, a quarter of the gaps between fresh
/// readings ran past a second and the longest was 2.1 s. So anything under about
/// 3 s would announce a signal loss during ordinary reception, which is what
/// picked this. Note that extrapolating this far isn't the inaccurate part — a
/// measured rate is only trusted within 500 ppm of nominal, which holds five
/// seconds of it to about a millisecond. The reason to stop is that a device
/// switched off looks exactly like a device that's just quiet, and after a few
/// seconds it's more likely the former.
pub const HOLDOVER: Duration = Duration::from_secs(5);

/// Baseline for measuring the device's rate against the host clock. Long,
/// because the measurement divides anchor jitter by it: over 10 s, a
/// sub-millisecond anchor error is well under 100 ppm, comfortably finer than
/// the drift being measured.
const RATE_BASELINE: Duration = Duration::from_secs(10);

/// How much of each rate measurement to believe. The measurement is noisy even
/// over that baseline, so most of the old estimate is kept.
const RATE_GAIN: f64 = 0.25;

/// Cap on how far the measured rate may sit from nominal. Two crystals disagree
/// by tens of ppm; past this it's measurement noise, not drift.
///
/// Getting the rate right buys less than it looks like it should — the slew loop
/// cancels a constant rate error to within a fraction of a millisecond between
/// adverts. Where it earns its keep is extrapolating across a gap in reception,
/// which is also why the cap matters: at [`HOLDOVER`], being 500 ppm wrong costs
/// about a millisecond, so a bad estimate can't do real harm either.
const MAX_RATE_ERROR: f64 = 500e-6;

/// What the smoothed clock says.
///
/// The two variants are the two states worth drawing differently: a clock
/// that's keeping time, and one that has stopped and is telling you so. There
/// is no third state for "running but stale" — that's what the whole of
/// [`HOLDOVER`] is, and it's indistinguishable from healthy bursty reception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// Free-running from a recent advertisement. The sub-frame position is
    /// interpolated, so this advances every time it's asked for.
    ///
    /// Consecutive samples inside one frame differ only in
    /// [`Timecode::subframe`], so anything displaying whole frames will draw
    /// the same line several times over. That's the intended shape: sample at
    /// your refresh rate and let the frame number change when it changes.
    Running(Timecode),

    /// Nothing has arrived for longer than [`HOLDOVER`], so extrapolation has
    /// stopped. This is the last timecode actually received, and how long ago.
    ///
    /// `last` is a real reading, not an extrapolated one — it's the frame the
    /// device actually sent, so it's safe to show as the last known good
    /// position. It stays put while `since` grows, and a display wanting to say
    /// "no signal for 6.1s" has both halves here.
    ///
    /// This state is sticky until something arrives: a device switched off and
    /// a device that's merely quiet look identical, so nothing here decides
    /// which it was.
    Lost { last: Timecode, since: Duration },
}

/// A local clock anchored to the Tentacle's advertisements.
///
/// Start one with [`Default`], feed it [`anchor`](FreeRun::anchor) as readings
/// arrive, and ask it [`sample`](FreeRun::sample) whenever you want to know
/// what to show. See the [module docs](self#using-it) for the shape of that
/// loop and the things worth getting right.
///
/// **One per device.** The clock models a single timeline; anchoring it from
/// two boxes doesn't average them, it makes each anchor a discontinuity to the
/// other. The scanner in [`ble`](crate::ble) keys one of these per
/// peripheral, which is the arrangement to copy.
///
/// A fresh one holds no state at all and [`sample`](FreeRun::sample) answers
/// `None`, so it costs nothing to make one per peripheral in range and find
/// out later which of them were Tentacles.
#[derive(Debug, Default)]
pub struct FreeRun {
    state: Option<State>,
}

impl FreeRun {
    /// Takes an advertisement as an anchor.
    ///
    /// `at` wants to be when the packet arrived rather than whenever it's
    /// convenient to call this: the whole scheme rests on pairing a reading with
    /// the host time it represents. Stamp [`Instant::now`] as the packet comes
    /// off the wire, before any await or lookup, and pass that.
    ///
    /// Call it for every reading. There's no need to filter or rate-limit —
    /// jitter averaging is the reason it wants them all, and a duplicate
    /// advertisement is just an anchor with nothing to correct.
    ///
    /// # What an anchor does
    ///
    /// Usually half of one anchor's error, and deliberately only half: the rest
    /// is left to the anchors after it, so jitter averages out instead of being
    /// chased. Four cases aren't ordinary, and all four are decided here rather
    /// than by the caller:
    ///
    /// - **The first one** starts the clock, at the nominal frame rate.
    /// - **A different [`Rate`]** means another device or a reconfigured one,
    ///   so nothing learned so far applies and the model starts again. This is
    ///   why one clock per device is the rule rather than a suggestion.
    /// - **An error past half a second** is the timecode being changed on the
    ///   device, or a jam-sync — something real rather than jitter — so the
    ///   clock snaps to it instead of spending a minute slewing there.
    /// - **The first anchor after a silence** past [`HOLDOVER`] reseats the
    ///   clock on the new reading while keeping the measured rate, since the
    ///   crystals didn't change while the signal was gone. A device coming back
    ///   resumes cleanly rather than slewing across however long it was away.
    ///
    /// Nothing here reports which case it took. If you need to know that a
    /// device went quiet, watch for [`Reading::Lost`] from
    /// [`sample`](FreeRun::sample), or let the [`ble`](crate::ble) scanner's
    /// `Event::Lost` tell you.
    pub fn anchor(&mut self, tc: &Timecode, at: Instant) {
        let Some(state) = &mut self.state else {
            self.state = Some(State::new(tc, tc.frame_position(), at, tc.rate.fps as f64));
            return;
        };
        if state.frame_rate != tc.rate {
            // Another rate means another device, or one that's been
            // reconfigured; nothing learned so far still applies.
            self.state = Some(State::new(tc, tc.frame_position(), at, tc.rate.fps as f64));
            return;
        }

        let predicted = state.extrapolate(at);
        let position = unwrap_day(tc.frame_position(), predicted, tc.rate);
        let error = position - predicted;

        // Coming back from a dropout, the model has been sitting still while the
        // device kept going, and a timecode that was set on the device is a
        // genuine discontinuity. Neither is something to slew towards: reseat on
        // the new reading, keeping the rate, since the crystals didn't change.
        let resumed = at.saturating_duration_since(state.anchored) > HOLDOVER;
        if resumed || error.abs() > SNAP.as_secs_f64() * state.rate {
            let rate = state.rate;
            self.state = Some(State::new(tc, position, at, rate));
            return;
        }

        state.measure_rate(position, at);
        state.pos = predicted + SLEW_GAIN * error;
        state.at = at;
        state.anchored = at;
        state.last = *tc;
    }

    /// What to show now. `None` until the first advertisement has landed.
    ///
    /// Call this at whatever rate you're drawing at — that's the point of the
    /// module. It does no I/O and holds no lock, so it's cheap enough to call
    /// per frame.
    ///
    /// `None` means this has never been anchored, which for a scan is how a
    /// peripheral that isn't a Tentacle answers: most things in the room never
    /// send timecode. It is not an error and it isn't the same as
    /// [`Reading::Lost`], which is a device that *was* sending and stopped.
    ///
    /// Takes `&mut self` because sampling advances the clock's own floor: it
    /// never hands out a position below the last one it gave, which is what
    /// keeps a mid-slew correction from reaching a display as a timecode that
    /// ticks backwards. Two consequences worth knowing:
    ///
    /// - `now` should not go backwards between calls. An earlier `now` is
    ///   clamped to the previous reading rather than rewinding, so an unordered
    ///   pair of callers gets a stalled clock instead of a wrong one.
    /// - The clock can stall for as long as a correction takes — never more
    ///   than a frame — and that stall is the intended behaviour, not a
    ///   dropped sample.
    pub fn sample(&mut self, now: Instant) -> Option<Reading> {
        let state = self.state.as_mut()?;

        let idle = now.saturating_duration_since(state.anchored);
        if idle > HOLDOVER {
            return Some(Reading::Lost {
                last: state.last,
                since: idle,
            });
        }

        // Slewing can move the model backwards by a few milliseconds; refusing
        // to hand out anything lower than last time is what keeps that from
        // reaching the display as a timecode that ticks backwards. The stall
        // lasts as long as the correction, which is never more than a frame.
        let position = state.extrapolate(now).max(state.last_out);
        state.last_out = position;
        Some(Reading::Running(Timecode::at_frame_position(
            position,
            state.frame_rate,
        )))
    }

    /// The last advertisement actually received, whatever the clock is doing.
    ///
    /// This is the un-smoothed truth: the frame the device sent, with its own
    /// sub-frame field, not extrapolated or slewed. Use it when you want what
    /// arrived rather than what to show — logging, or checking the clock
    /// against its input. [`sample`](FreeRun::sample) is what a display wants.
    ///
    /// Unlike `sample` this takes `&self` and never advances anything, so it's
    /// safe to read as often as you like.
    pub fn last_received(&self) -> Option<Timecode> {
        self.state.as_ref().map(|s| s.last)
    }
}

#[derive(Debug)]
struct State {
    /// The rate the device is running at, as it named it. Distinct from
    /// `rate` below, which is how fast it turns out to be running in host
    /// seconds.
    frame_rate: Rate,
    /// The model of the device's timeline: `pos` frames at `at`, advancing at
    /// `rate` frames per second of host time.
    pos: f64,
    at: Instant,
    rate: f64,
    /// Highest position handed out so far.
    last_out: f64,
    /// The last advertisement, and when it reached us.
    last: Timecode,
    anchored: Instant,
    /// Where the rate measurement is currently counting from.
    rate_from: (f64, Instant),
}

impl State {
    fn new(tc: &Timecode, position: f64, at: Instant, rate: f64) -> Self {
        State {
            frame_rate: tc.rate,
            pos: position,
            at,
            rate,
            last_out: position,
            last: *tc,
            anchored: at,
            rate_from: (position, at),
        }
    }

    /// Where the model says the device is at `now`.
    fn extrapolate(&self, now: Instant) -> f64 {
        self.pos + secs_between(now, self.at) * self.rate
    }

    /// Re-measures how fast the device runs against the host clock, over a
    /// baseline long enough for anchor jitter to average out of it.
    fn measure_rate(&mut self, position: f64, at: Instant) {
        let (from_position, from_at) = self.rate_from;
        let span = secs_between(at, from_at);
        if span < RATE_BASELINE.as_secs_f64() {
            return;
        }
        let nominal = self.frame_rate.fps as f64;
        let measured = ((position - from_position) / span).clamp(
            nominal * (1.0 - MAX_RATE_ERROR),
            nominal * (1.0 + MAX_RATE_ERROR),
        );
        self.rate += RATE_GAIN * (measured - self.rate);
        self.rate_from = (position, at);
    }
}

/// Puts a reading on the same side of midnight as the running model.
///
/// Frame positions wrap every 24 hours while the model just keeps counting, so
/// shifting a reading by whole days to land nearest the prediction is what keeps
/// the two comparable. The extra days come back off in
/// [`Timecode::at_frame_position`], which is modulo a day anyway.
fn unwrap_day(position: f64, predicted: f64, rate: Rate) -> f64 {
    let day = rate.frames_per_day();
    position + ((predicted - position) / day).round() * day
}

/// Seconds from `earlier` to `later`, negative if they're the other way round.
///
/// Anchors are timestamped on arrival and sampling happens on a timer, so the
/// two orders do interleave; [`Instant`] subtraction would saturate at zero and
/// quietly bias the result.
fn secs_between(later: Instant, earlier: Instant) -> f64 {
    match later.checked_duration_since(earlier) {
        Some(span) => span.as_secs_f64(),
        None => -earlier.duration_since(later).as_secs_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u8 = 25;
    const RATE: Rate = Rate {
        fps: FPS,
        drop_frame: false,
    };
    const FRAME: f64 = 1.0 / FPS as f64;

    fn at(seconds: f64) -> Timecode {
        Timecode::at_frame_position(seconds * FPS as f64, RATE)
    }

    /// The clock's position, in seconds since midnight on the device's timeline.
    fn seconds_shown(clock: &mut FreeRun, now: Instant) -> f64 {
        match clock.sample(now) {
            Some(Reading::Running(tc)) => tc.frame_position() / FPS as f64,
            other => panic!("expected a running reading, got {other:?}"),
        }
    }

    fn millis(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Deterministic stand-in for Bluetooth delivery jitter, ±`spread` seconds.
    struct Jitter(u64);

    impl Jitter {
        fn next(&mut self, spread: f64) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            (unit * 2.0 - 1.0) * spread
        }
    }

    #[test]
    fn extrapolates_between_anchors() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);

        // Nothing more arrives, but the clock keeps going.
        assert!((seconds_shown(&mut clock, t0) - 10.0).abs() < FRAME);
        assert!((seconds_shown(&mut clock, t0 + millis(500)) - 10.5).abs() < FRAME);
        assert!((seconds_shown(&mut clock, t0 + millis(1200)) - 11.2).abs() < FRAME);
    }

    #[test]
    fn shows_every_frame_in_between() {
        // The point of the exercise: sampling at the frame rate between adverts
        // half a second apart has to walk the frames rather than jump.
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);
        clock.anchor(&at(10.5), t0 + millis(500));

        let mut frames = vec![];
        for step in 0..13 {
            let now = t0 + millis(500 + step * 40);
            if let Some(Reading::Running(tc)) = clock.sample(now) {
                frames.push(tc.frames);
            }
        }
        frames.dedup();
        assert_eq!(frames, (12..=24).collect::<Vec<u8>>());
    }

    #[test]
    fn jitter_does_not_snap_the_clock() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);

        // An anchor 10 ms behind where the clock has got to — far worse than
        // reception really is — must not drag the clock all the way back.
        let before = seconds_shown(&mut clock, t0 + millis(500));
        clock.anchor(&at(10.49), t0 + millis(500));
        let after = seconds_shown(&mut clock, t0 + millis(500));
        let moved = before - after;
        assert!(
            moved <= 0.0 || moved < 0.01 * (1.0 - SLEW_GAIN) + 1e-6,
            "a 10 ms error moved the clock {} ms",
            moved * 1000.0
        );
    }

    #[test]
    fn never_ticks_backwards() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        let mut jitter = Jitter(1);
        let mut shown = 0.0_f64;

        // Two minutes of adverts landing every ~600 ms, each ±5 ms out — far
        // more error than the sub-frame field actually leaves, so this is the
        // property under stress rather than under realistic conditions.
        for tick in 0..12_000_u64 {
            let now = t0 + millis(tick * 10);
            let elapsed = tick as f64 / 100.0;
            if tick % 60 == 0 {
                let error = jitter.next(0.005);
                clock.anchor(&at(10.0 + elapsed + error), now);
            }
            let position = seconds_shown(&mut clock, now);
            assert!(
                position >= shown,
                "went backwards at {elapsed}s: {shown} then {position}"
            );
            shown = position;
        }
    }

    #[test]
    fn stays_on_the_devices_timeline() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        let mut jitter = Jitter(7);

        // Same run with the ±1 ms the anchors really carry, checked for accuracy
        // rather than monotonicity. Smoothing trades a little of it away; the
        // trade has to stay far inside a frame, not merely within one.
        for tick in 0..12_000_u64 {
            let now = t0 + millis(tick * 10);
            let elapsed = tick as f64 / 100.0;
            if tick % 60 == 0 {
                let error = jitter.next(0.001);
                clock.anchor(&at(10.0 + elapsed + error), now);
            }
            let position = seconds_shown(&mut clock, now);
            assert!(
                (position - (10.0 + elapsed)).abs() < 0.003,
                "off by {} ms at {elapsed}s",
                (position - (10.0 + elapsed)) * 1000.0
            );
        }
    }

    #[test]
    fn follows_a_device_whose_clock_drifts() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        // 200 ppm fast is more than a real crystal, and over five minutes it's
        // 60 ms — a frame and a half at 25 fps — of walk-off if nothing tracks
        // it. Between them the slew loop and the rate estimate must leave none
        // of it accumulating.
        let drift = 1.0 + 200e-6;

        for tick in 0..30_000_u64 {
            let now = t0 + millis(tick * 10);
            let elapsed = tick as f64 / 100.0;
            if tick % 60 == 0 {
                clock.anchor(&at(10.0 + elapsed * drift), now);
            }
            let position = seconds_shown(&mut clock, now);
            assert!(
                (position - (10.0 + elapsed * drift)).abs() < 0.005,
                "off by {} ms at {elapsed}s",
                (position - (10.0 + elapsed * drift)) * 1000.0
            );
        }
    }

    #[test]
    fn says_so_when_the_signal_goes() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);

        // Still extrapolating just inside the holdover.
        assert!(matches!(
            clock.sample(t0 + HOLDOVER - millis(1)),
            Some(Reading::Running(_))
        ));

        // Past it, it stops inventing timecode and reports the last real
        // reading instead of where it thinks the device has got to.
        let Some(Reading::Lost { last, since }) = clock.sample(t0 + HOLDOVER + millis(1500)) else {
            panic!("kept extrapolating past the holdover");
        };
        assert_eq!(last.to_string(), at(10.0).to_string());
        assert!(since > HOLDOVER);
    }

    #[test]
    fn resumes_from_where_the_device_got_to() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);

        // Ten seconds off the air, then adverts come back. The clock has to jump
        // to the device rather than slew ten seconds towards it.
        let back = t0 + Duration::from_secs(10);
        clock.anchor(&at(20.0), back);
        assert!((seconds_shown(&mut clock, back) - 20.0).abs() < FRAME);
        assert!((seconds_shown(&mut clock, back + millis(500)) - 20.5).abs() < FRAME);
    }

    #[test]
    fn snaps_when_the_timecode_is_changed_on_the_device() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(3600.0), t0);
        clock.anchor(&at(3600.6), t0 + millis(600));

        // Jam-syncing to a different hour is a real discontinuity, backwards or
        // not, and has to show up immediately.
        let jammed = t0 + millis(1200);
        clock.anchor(&at(60.0), jammed);
        assert!((seconds_shown(&mut clock, jammed) - 60.0).abs() < FRAME);
        assert!((seconds_shown(&mut clock, jammed + millis(400)) - 60.4).abs() < FRAME);
    }

    #[test]
    fn keeps_running_over_midnight() {
        // Adverts either side of 00:00:00:00, where the position the arithmetic
        // works in wraps back to zero.
        let day = RATE.frames_per_day() / FPS as f64;
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(day - 0.6), t0);
        clock.anchor(&at(day - 0.1), t0 + millis(500));

        let Some(Reading::Running(tc)) = clock.sample(t0 + millis(900)) else {
            panic!("lost the clock at midnight");
        };
        assert_eq!(tc.to_string(), "00:00:00:07");

        // And the advert that lands after the wrap must not read as an
        // eleven-hour discontinuity and snap.
        clock.anchor(&at(0.4), t0 + millis(1000));
        let Some(Reading::Running(tc)) = clock.sample(t0 + millis(1000)) else {
            panic!("lost the clock after midnight");
        };
        assert_eq!(tc.to_string(), "00:00:00:10");
    }

    #[test]
    fn a_change_of_frame_rate_starts_over() {
        let t0 = Instant::now();
        let mut clock = FreeRun::default();
        clock.anchor(&at(10.0), t0);

        let thirty = Timecode::at_frame_position(10.0 * 30.0, Rate::whole(30));
        clock.anchor(&thirty, t0 + millis(500));
        let Some(Reading::Running(tc)) = clock.sample(t0 + millis(500)) else {
            panic!("lost the clock");
        };
        assert_eq!(tc.rate.fps, 30);
        assert_eq!(tc.to_string(), "00:00:10:00");
    }

    #[test]
    fn nothing_to_show_before_the_first_advert() {
        assert!(FreeRun::default().sample(Instant::now()).is_none());
        assert!(FreeRun::default().last_received().is_none());
    }
}
