//! Calibrating how far behind a Tentacle's own clock a reading lands.
//!
//! A timecode box jam syncs from a master and then free-runs. This does the
//! same thing to a host clock, with one difference worth being precise about:
//! **classic jam sync takes the *time* once and then drifts. This takes the
//! *path latency* once and keeps taking the time from advertisements forever.**
//!
//! The calibrated quantity is a path constant — this host's Bluetooth stack
//! floor, plus the bias in the origin of the device's microsecond counter — and
//! a path constant does not drift with the crystals. So this is *calibrate
//! once, then track*, not calibrate and decay. Nothing here needs redoing as
//! the run goes on, and nothing here is a time transfer: the time keeps coming
//! from the advertisements, exactly as it did before.
//!
//! [`Calibrator`] collects samples and [`Calibration`] is what falls out.
//! Everything here is arithmetic over samples somebody else collected, so it is
//! always available and can be tested without hardware. Taking the samples
//! needs Bluetooth, and `shokushu-gatt --phase` — which needs `scan` and `cli`
//! — is what takes them.
//!
//! # What this is for, which is mostly not the correction
//!
//! Feeding a [`Calibration::offset`] to
//! [`FreeRun::jam`](crate::freerun::FreeRun::jam) is available and is not the
//! point. The measurement in `PROTOCOL.md` puts a free-running clock
//! **already** within about 2.09 ms of the device; applying the midpoint of
//! that bracket cuts the worst case to about 1.05 ms. That is 0.05 of a frame
//! at 24 fps against 0.025 — both so far inside the half-frame that decides a
//! displayed frame number that neither is visible. The constant also cancels
//! outright in the three things this crate mostly does: measuring drift (a
//! difference of two anchors), comparing two boxes to each other (the same host
//! path both times), and showing frames.
//!
//! **The reason this module exists is to keep that claim checkable.** "A
//! free-running advertisement is under 2 ms behind" is a measurement, on one
//! box, on one host, on one firmware. It should be re-runnable on new hardware,
//! after a firmware change, or when somebody doubts it — and by something with
//! tests rather than by rerunning a script and squinting. So the value here is
//! in the refusals: the bracket that cannot come out negative, the drift fit
//! that one bad block cannot own, the pooling that is illegitimate until the
//! drift comes out, and the two advertisement populations that must be compared
//! at matched counts. Those are what stop a plausible wrong answer, and one of
//! them has already caught one.
//!
//! It cannot do better than the bracket, because the bracket is floored by the
//! 30 ms connection interval, which macOS gives no way to negotiate down.
//! Anything wanting sub-millisecond wants LTC, which is a separately
//! calibratable transport where a Bluetooth stack is not.
//!
//! # Nothing here connects to anything
//!
//! Deliberately. A box holding a link advertises late and less, refuses
//! connections after a few dozen rapid ones, and there is an open question
//! about whether being connected to can knock a box off a timeline it shares
//! with others. Connecting to a box that is in service to buy back 0.025 of a
//! frame is a bad trade, so this module never does it: it reduces samples from
//! a capture that was taken deliberately, by a tool the operator ran on a box
//! they chose.
//!
//! # The arithmetic
//!
//! An ATT read is a round trip and an advertisement structurally cannot be.
//! Stamp the host clock either side of a read and the timecode in the response
//! stands in for the device's own two stamps:
//!
//! ```text
//!   t0 ---- Read Request ---> | device stamps T | ---- Read Response ---> t1
//! ```
//!
//! Write `a = T - t0` and `b = t1 - T`, and let `θ` be the device's clock minus
//! this host's. Then `a = d_out + θ` and `b = d_ret - θ` for the two one-way
//! delays. Neither delay is knowable alone, but both are elapsed times and so
//! both are at least zero, which gives
//!
//! ```text
//!   θ ≤ a        and        θ ≥ -b
//! ```
//!
//! for *every single sample*. `min(a)` bounds the offset above and `-min(b)`
//! bounds it below, with **no assumption that the two legs are equal** — and
//! they are conspicuously not, since a request waits for the next connection
//! anchor point and a response, already at the device, does not. Halving the
//! round trip is NTP's move and it would take that asymmetry on as bias, of the
//! same order as the effect being measured. The bracket is the honest form.
//!
//! A one-way stream has no departure stamp, so all it gives is the `b` half.
//! But `θ` is common to every stream, so differencing two streams' `b` floors
//! cancels it and leaves the delivery cost of one path against another. That is
//! the bridge from the connection path, where the round trip lives, onto the
//! advertisement path, which is the one [`freerun`](crate::freerun) actually
//! uses.
//!
//! # The absolute numbers mean nothing
//!
//! Host time here is an interval off an [`Instant`] and device time is a time
//! of day, so `a` and `b` are enormous numbers with no meaning of their own.
//! Only differences of differences mean anything, which is why every figure
//! this module reports is a width, a floor measured against another floor, or a
//! rate — and why there is deliberately no way to ask a [`Calibration`] what
//! `θ` was. [`Calibration::bracket_width`] is its width and that is all.
//!
//! # The trap this module exists to avoid
//!
//! **Connecting to a box delays that box's own advertisements by about
//! 9.5 ms.** A box holds a link *and* advertises, and the advertising loses. An
//! earlier version of this measurement reported 6–10 ms from captures whose
//! advertisements were all taken while a link was up to the very box being
//! measured; the number was a measurement of the observer.
//!
//! So a calibration has to be interleaved — connect for round trips,
//! disconnect, let the box advertise normally, and carry `θ` across the gap on
//! the measured drift, which at 8.6 ppm moves it 0.09 ms in ten seconds. Two
//! things here enforce it. [`Link`] is an enum rather than a `bool` because
//! getting it backwards is exactly the mistake that produced the wrong answer;
//! and [`Calibration::suppression`] compares the two populations **at matched
//! sample counts**, because a minimum over fewer samples sits higher for that
//! reason alone and would otherwise look like a slower path. A capture holding
//! no free-running advertisements at all is refused outright — see
//! [`Flaw::NoFreeRunning`].

use std::fmt;
use std::time::{Duration, Instant};

use crate::timecode::Timecode;

/// The block length the bracket is computed over.
///
/// Short enough that drift inside one block is well under the bracket it is
/// measuring — 0.15 ms over 30 s at the rates two crystals here disagree by —
/// and long enough to hold a few connections' worth of round trips.
pub const BLOCK: Duration = Duration::from_secs(30);

/// The connection interval, and so the least a real round trip can take.
///
/// `PROTOCOL.md` measures it at 30 ms from the notification gap histogram —
/// 3,076 of 3,091 gaps within 4 ms of a multiple of it. Used only to recognise
/// a "round trip" too short to be one, which is the signature of a read taken
/// while subscribed. [`Calibrator::with_interval`] overrides it if a box ever
/// negotiates another.
pub const CONNECTION_INTERVAL: Duration = Duration::from_millis(30);

/// How much of the bracket the advertisement floor may still be moving by and
/// still count as [`settled`](Calibration::settled).
///
/// A judgement, not a measurement. Five per cent of the bracket width is about
/// 0.17 ms on the captures this was built against — comfortably below anything
/// the bracket resolves, and comfortably above the noise in a floor that has
/// genuinely stopped falling. The rule it feeds is stated in `settled`.
pub const SETTLED_FRACTION: f64 = 0.05;

/// Whether a link was up when an advertisement landed.
///
/// An enum and not a `bool` because getting it backwards is exactly the mistake
/// that produced the wrong first answer to this question — every advertisement
/// in those captures was caught while the tool held a connection to the box it
/// was measuring, and nothing in the capture looked wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Link {
    /// A connection to this box was open. Its advertising is suppressed and
    /// delayed; see the module docs.
    Up,
    /// No connection, so the box is advertising as a passive listener sees it.
    /// This is the population a calibration can honestly be built from.
    Down,
}

/// Which path a reading reached the host by.
///
/// Output-side: [`Stream::Read`] is what [`Calibrator::round_trip`] produces
/// and the rest are what the caller reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stream {
    /// An ATT read response — the only stream that bounds `θ` from above,
    /// because it is the only one with a departure stamp.
    Read,
    /// A GATT notification. Real, and its arrival time says what the
    /// notification path costs, but it is not a round trip.
    Notify,
    /// An advertisement that landed while a link was up to the box.
    ///
    /// Here to be compared against [`Stream::Advert`], not to be used: a floor
    /// measured under a connection is a floor for a state a passive clock is
    /// never in.
    ConnectedAdvert,
    /// A free-running advertisement — the path [`freerun`](crate::freerun)
    /// actually uses, and the only one whose constant may honestly be applied
    /// to a passive clock.
    Advert,
}

impl Stream {
    /// Every stream, in report order.
    const ALL: [Stream; 4] = [
        Stream::Read,
        Stream::Notify,
        Stream::ConnectedAdvert,
        Stream::Advert,
    ];

    fn label(self) -> &'static str {
        match self {
            Stream::Read => "read",
            Stream::Notify => "notify",
            Stream::ConnectedAdvert => "advert (connected)",
            Stream::Advert => "advert (free-running)",
        }
    }
}

/// An interval a quantity provably lies in.
///
/// Both ends are non-negative durations by construction — see
/// [`Calibration::staleness`] for why that is a property of the arithmetic
/// rather than a clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bracket {
    pub low: Duration,
    pub high: Duration,
}

impl Bracket {
    /// How much is not known. This is the uncertainty, and the thing the 30 ms
    /// connection interval puts a floor under.
    pub fn width(&self) -> Duration {
        self.high.saturating_sub(self.low)
    }

    /// The middle of the interval — a midpoint, and not a measurement. It is
    /// the best single number to apply if a single number must be applied, and
    /// applying it is what halves the worst case.
    pub fn midpoint(&self) -> Duration {
        self.low + self.width() / 2
    }
}

impl fmt::Display for Bracket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.3} to {:.3} ms",
            self.low.as_secs_f64() * 1e3,
            self.high.as_secs_f64() * 1e3
        )
    }
}

/// Why a capture could not be reduced to a [`Calibration`].
///
/// Every one of these is a refusal rather than a warning, and deliberately so:
/// every figure in this module is a *minimum*, so a single impossible sample
/// does not perturb the answer, it silently becomes the answer. There is one
/// split worth knowing — a capture *tool* warns at capture time, because a
/// half-spoiled capture still holds real samples; this harness refuses,
/// because by the time it runs there is nothing left to salvage by continuing.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Flaw {
    /// No round trips at all, so nothing bounds `θ` from above and nothing here
    /// can be computed.
    NoRoundTrips,

    /// The device's clock stepped relative to the host's — a midnight wrap, or
    /// the box being re-jammed mid-capture.
    ClockJumped {
        /// How far into the capture the step happened.
        at: Duration,
        /// The size of the step, in seconds, signed.
        by: f64,
    },

    /// Too many "round trips" came back faster than a connection interval,
    /// which a real one cannot do.
    ///
    /// The signature of a capture taken while subscribed: on macOS a Read
    /// Response and a notification arrive through the same delegate callback,
    /// so a pending read is resolved by whichever lands first. The value is
    /// genuine and the timing is fiction — 83.6% of one subscribed capture came
    /// back under one interval, the shortest in 31 µs.
    NotRoundTrips {
        short: usize,
        total: usize,
        shortest: Duration,
    },

    /// Every block's bracket came out negative, so nothing here bounds
    /// anything. See [`Calibration::contradictory_blocks`].
    EveryBlockContradictory { blocks: usize },

    /// Pooled across streams, the two bounds on `θ` cross: some stream reached
    /// the host sooner after being stamped than the round trips allow `θ` to
    /// be. The one-block sign check caught nothing, but the pooled one does.
    StreamsContradict {
        /// `min(a)` over the reads, de-trended.
        upper: f64,
        /// `min(b)` over every stream, de-trended.
        lower: f64,
    },

    /// The capture holds no free-running advertisements, so the one path a
    /// passive clock actually uses was never observed unperturbed.
    ///
    /// The fix is to rest between connections and keep scanning through the
    /// rest. A constant measured only while connected is a constant for a state
    /// [`freerun`](crate::freerun) is never in, and handing one back would be
    /// worse than refusing.
    NoFreeRunning,
}

impl fmt::Display for Flaw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Flaw::NoRoundTrips => write!(
                f,
                "no round trips in the capture: nothing bounds the offset from above"
            ),
            Flaw::ClockJumped { at, by } => write!(
                f,
                "the device clock jumped {by:+.1} s at {:.1} s in — a midnight wrap or a \
                 re-jam. Every figure here is a minimum, so one such step becomes the \
                 answer; split the capture and re-run",
                at.as_secs_f64()
            ),
            Flaw::NotRoundTrips {
                short,
                total,
                shortest,
            } => write!(
                f,
                "{short} of {total} round trips came back in under one connection \
                 interval, the shortest in {:.3} ms, which a real one cannot do. A read \
                 taken while subscribed is resolved by the notification stream, not by a \
                 Read Response, so its timing is fiction — re-capture without subscribing",
                shortest.as_secs_f64() * 1e3
            ),
            Flaw::EveryBlockContradictory { blocks } => write!(
                f,
                "all {blocks} block(s) came out with a negative bracket, which is \
                 impossible for a real round trip: each holds at least one sample whose \
                 device stamp predates the request that returned it"
            ),
            Flaw::StreamsContradict { upper, lower } => write!(
                f,
                "the pooled bounds on the offset cross: min(a) over the reads is \
                 {:.3} ms against a floor of {:.3} ms over every stream, so no value of \
                 the offset satisfies both",
                upper * 1e3,
                lower * 1e3
            ),
            Flaw::NoFreeRunning => write!(
                f,
                "no free-running advertisements in the capture, so the path a passive \
                 clock uses was never observed unperturbed. Rest between connections and \
                 keep scanning through the rest"
            ),
        }
    }
}

impl std::error::Error for Flaw {}

/// One reading, reduced to the quantities the arithmetic wants.
///
/// Everything is f64 seconds: `host` is an interval since the calibrator's
/// origin and `device` is a device time of day, so neither is meaningful alone
/// and only their differences are used.
#[derive(Debug, Clone, Copy)]
struct Sample {
    stream: Stream,
    /// When it landed, seconds since the origin. For a round trip this is `t1`.
    host: f64,
    /// The device's own stamp, seconds since its midnight.
    device: f64,
    /// `T - t0`, present only for a round trip. Bounds `θ` from above.
    a: Option<f64>,
    /// `arrival - T`. Every stream has one. Bounds `θ` from below.
    b: f64,
    /// `t1 - t0`, present only for a round trip.
    rtt: Option<f64>,
    /// `a` with the fitted drift taken out; see `detrend`.
    a_flat: f64,
    /// `b` with the fitted drift taken out.
    b_flat: f64,
}

/// Collects readings, and reduces them to a [`Calibration`].
///
/// Three explicit methods rather than one that takes a [`Stream`], so that the
/// [`Link`] on an advertisement cannot be forgotten. Feed it whatever a capture
/// produces and call [`finish`](Calibrator::finish); the checks that decide
/// whether the capture is usable at all live there.
#[derive(Debug)]
pub struct Calibrator {
    origin: Instant,
    block: Duration,
    interval: Duration,
    samples: Vec<Sample>,
    undecoded: usize,
}

impl Default for Calibrator {
    fn default() -> Calibrator {
        Calibrator::new()
    }
}

impl Calibrator {
    /// A fresh calibrator, with its origin at this instant.
    ///
    /// The origin is only a reference for the host-time axis — every figure is
    /// a difference, so where it sits does not matter, only that all the
    /// samples share it.
    pub fn new() -> Calibrator {
        Calibrator::since(Instant::now())
    }

    /// The same, with the origin given rather than taken now.
    ///
    /// For feeding samples whose host stamps were taken against a clock that
    /// was already running — a capture being replayed, or a tool that has been
    /// timing something else since before the calibration started. Only the
    /// samples sharing one origin matters; where it sits does not.
    pub fn since(origin: Instant) -> Calibrator {
        Calibrator {
            origin,
            block: BLOCK,
            interval: CONNECTION_INTERVAL,
            samples: Vec::new(),
            undecoded: 0,
        }
    }

    /// Sets the block length the per-block brackets are computed over.
    ///
    /// See [`BLOCK`] for what the default trades off. Shorter blocks hold less
    /// drift and fewer samples; longer ones the reverse.
    pub fn with_block(mut self, block: Duration) -> Calibrator {
        self.block = block;
        self
    }

    /// Sets the connection interval used to recognise a round trip too short to
    /// be one. See [`CONNECTION_INTERVAL`].
    pub fn with_interval(mut self, interval: Duration) -> Calibrator {
        self.interval = interval;
        self
    }

    /// A round trip: the timecode that came back, and the host stamps either
    /// side of the read that fetched it.
    ///
    /// `sent` and `received` want to be stamped with nothing between them and
    /// the read but the read itself. This is the only input that bounds `θ`
    /// from above, so a capture without any of these cannot be reduced at all.
    pub fn round_trip(&mut self, tc: &Timecode, sent: Instant, received: Instant) {
        let device = device_seconds(tc);
        let t0 = self.secs(sent);
        let t1 = self.secs(received);
        self.samples.push(Sample {
            stream: Stream::Read,
            host: t1,
            device,
            a: Some(device - t0),
            b: t1 - device,
            rtt: Some(t1 - t0),
            a_flat: 0.0,
            b_flat: 0.0,
        });
    }

    /// A GATT notification, which has an arrival stamp and no departure one.
    pub fn notification(&mut self, tc: &Timecode, at: Instant) {
        self.one_way(Stream::Notify, tc, at);
    }

    /// An advertisement, and whether a link was up to this box when it landed.
    ///
    /// The [`Link`] is the whole reason this is not the same call as
    /// [`notification`](Calibrator::notification): a connected box's
    /// advertisements land about 9.5 ms later against its own stamp, and
    /// pooling the two populations is how the first answer to this question
    /// came out wrong.
    pub fn advert(&mut self, tc: &Timecode, at: Instant, link: Link) {
        let stream = match link {
            Link::Up => Stream::ConnectedAdvert,
            Link::Down => Stream::Advert,
        };
        self.one_way(stream, tc, at);
    }

    /// A payload that arrived under the right service and did not decode.
    ///
    /// Counted rather than ignored, so a capture that was mostly undecodable
    /// says so in its report instead of looking clean.
    pub fn undecoded(&mut self) {
        self.undecoded += 1;
    }

    /// How many samples have been taken in, of every kind.
    pub fn samples(&self) -> usize {
        self.samples.len()
    }

    fn one_way(&mut self, stream: Stream, tc: &Timecode, at: Instant) {
        let device = device_seconds(tc);
        let host = self.secs(at);
        self.samples.push(Sample {
            stream,
            host,
            device,
            a: None,
            b: host - device,
            rtt: None,
            a_flat: 0.0,
            b_flat: 0.0,
        });
    }

    fn secs(&self, at: Instant) -> f64 {
        secs_between(at, self.origin)
    }

    /// Reduces everything collected so far, or says why it cannot be.
    ///
    /// The checks run in the order they are cheapest to explain, and each one
    /// is a refusal because everything downstream of it is a minimum. In order:
    /// there must be round trips at all; the device clock must not have
    /// stepped; the round trips must actually be round trips; blocks whose
    /// bracket is impossible are dropped along with their reads; the drift is
    /// fitted robustly and taken out before anything is pooled; and the capture
    /// must hold free-running advertisements, or the constant it would hand
    /// back describes a state no passive clock is ever in.
    pub fn finish(&self) -> Result<Calibration, Flaw> {
        let mut reads: Vec<Sample> = self
            .samples
            .iter()
            .copied()
            .filter(|s| s.stream == Stream::Read)
            .collect();
        if reads.is_empty() {
            return Err(Flaw::NoRoundTrips);
        }
        reads.sort_by(|x, y| x.host.total_cmp(&y.host));

        // A timecode that wraps midnight — or a box re-jammed mid-capture —
        // moves the device clock by a whole day or steps it. Every figure below
        // is a minimum, so one such sample silently becomes the answer, and it
        // is impossible to spot in the output afterwards.
        for pair in reads.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            let step = (after.device - before.device) - (after.host - before.host);
            if step.abs() > 1.0 {
                return Err(Flaw::ClockJumped {
                    at: Duration::from_secs_f64(after.host.max(0.0)),
                    by: step,
                });
            }
        }

        // A read taken while subscribed is resolved by the notification stream
        // rather than by a Read Response, and the give-away is a round trip
        // shorter than one connection interval. Two per cent tolerates the
        // occasional oddity that real captures do show; past that the whole
        // reduction is meaningless and there is nothing to salvage.
        let interval = self.interval.as_secs_f64();
        let rtts: Vec<f64> = reads.iter().filter_map(|s| s.rtt).collect();
        let short = rtts.iter().filter(|&&r| r < interval).count();
        let shortest = rtts.iter().copied().fold(f64::INFINITY, f64::min);
        if short as f64 > 0.02 * reads.len() as f64 {
            return Err(Flaw::NotRoundTrips {
                short,
                total: reads.len(),
                shortest: Duration::from_secs_f64(shortest.max(0.0)),
            });
        }

        // Per-block brackets, and the drift their midpoints imply.
        let block = self.block.as_secs_f64().max(f64::MIN_POSITIVE);
        let mut blocks: Vec<BlockReport> = Vec::new();
        let mut index = 0usize;
        let mut bad_blocks: Vec<i64> = Vec::new();
        while index < reads.len() {
            let key = (reads[index].host / block).floor() as i64;
            let mut end = index;
            while end < reads.len() && (reads[end].host / block).floor() as i64 == key {
                end += 1;
            }
            let rows = &reads[index..end];
            let lower = -min_of(rows.iter().map(|s| s.b));
            let upper = min_of(rows.iter().filter_map(|s| s.a));
            let width = upper - lower;
            // `min(a) + min(b) = min(d_out) + min(d_ret)` is a sum of two
            // elapsed times and cannot be below zero. A block that comes out
            // negative holds at least one sample whose device stamp predates
            // the request that returned it — not a tighter bound but a broken
            // one, and its midpoint would drag the drift fit too.
            if width < 0.0 {
                bad_blocks.push(key);
            }
            blocks.push(BlockReport {
                at: Duration::from_secs_f64((key as f64 * block).max(0.0)),
                n: rows.len(),
                width,
                shortest_rtt: min_of(rows.iter().filter_map(|s| s.rtt)),
                midpoint: (lower + upper) / 2.0,
                contradictory: width < 0.0,
            });
            index = end;
        }
        let contradictory = bad_blocks.len();
        if contradictory == blocks.len() {
            return Err(Flaw::EveryBlockContradictory {
                blocks: blocks.len(),
            });
        }
        // Dropped entirely rather than merely flagged: everything past here is
        // a minimum, so one impossible sample would become the answer wherever
        // it pooled. Advertisements from the same window are kept — the
        // contradiction is a property of the round trips.
        reads.retain(|s| !bad_blocks.contains(&((s.host / block).floor() as i64)));
        if reads.is_empty() {
            return Err(Flaw::EveryBlockContradictory {
                blocks: blocks.len(),
            });
        }

        let midpoints: Vec<(f64, f64)> = blocks
            .iter()
            .filter(|b| !b.contradictory)
            .map(|b| (b.at.as_secs_f64() + block / 2.0, b.midpoint))
            .collect();
        let line = fit(&midpoints);
        let slope = line.map(|(s, _)| s).unwrap_or(0.0);
        // Two points define a line and say nothing about whether it is one, so
        // a rate is only quoted from three.
        let drift_ppm = (midpoints.len() >= 3).then_some(slope * 1e6);
        let drift_residual = line.filter(|_| midpoints.len() >= 3).map(|(s, c)| {
            let sum: f64 = midpoints.iter().map(|(x, y)| (y - (s * x + c)).powi(2)).sum();
            (sum / midpoints.len() as f64).sqrt()
        });

        // De-trend before anything is pooled. `a` rises with a positive drift
        // while `b` falls, so `min(a)` comes from early in the capture and
        // `min(b)` from late; the two bounds then constrain *different* values
        // of `θ` and the interval between them is spuriously narrow rather than
        // spuriously wide. Pooled raw over one real capture that read 1.75 ms
        // against a true 3.43.
        let flatten = |s: &Sample| Sample {
            a_flat: s.a.unwrap_or(f64::INFINITY) - slope * s.host,
            b_flat: s.b + slope * s.host,
            ..*s
        };
        let reads: Vec<Sample> = reads.iter().map(flatten).collect();
        let others: Vec<Sample> = self
            .samples
            .iter()
            .filter(|s| s.stream != Stream::Read)
            .map(flatten)
            .collect();

        let upper_read = min_of(reads.iter().map(|s| s.a_flat));
        let bracket_width = upper_read + min_of(reads.iter().map(|s| s.b_flat));

        // The lower bound on `θ` may come from any stream and should come from
        // whichever gives the tightest one: advertisements reach the host
        // sooner after being stamped than read responses do, so using the
        // reads' floor here would both loosen the bracket and report a negative
        // staleness, which no delivery delay can be.
        let lower_best = min_of(reads.iter().chain(others.iter()).map(|s| s.b_flat));
        if upper_read + lower_best < 0.0 {
            return Err(Flaw::StreamsContradict {
                upper: upper_read,
                lower: lower_best,
            });
        }

        // For each stream, the staleness of its *least delayed* reading: how
        // far behind the device's own clock that reading was when it landed,
        // which is the error left in a clock anchoring on the best reading it
        // sees. `d = b + θ`, with `b` measured and `θ` bracketed, so
        // `d ∈ [floor - lower_best, floor + upper_read]`.
        let mut staleness = Vec::new();
        for stream in Stream::ALL {
            let floor = min_of(
                reads
                    .iter()
                    .chain(others.iter())
                    .filter(|s| s.stream == stream)
                    .map(|s| s.b_flat),
            );
            if floor.is_infinite() {
                continue;
            }
            let count = reads
                .iter()
                .chain(others.iter())
                .filter(|s| s.stream == stream)
                .count();
            staleness.push((
                stream,
                count,
                Bracket {
                    low: Duration::from_secs_f64((floor - lower_best).max(0.0)),
                    high: Duration::from_secs_f64((floor + upper_read).max(0.0)),
                },
            ));
        }

        let free_running: Vec<Sample> = others
            .iter()
            .copied()
            .filter(|s| s.stream == Stream::Advert)
            .collect();
        if free_running.is_empty() {
            return Err(Flaw::NoFreeRunning);
        }

        let offset = staleness
            .iter()
            .find(|(stream, ..)| *stream == Stream::Advert)
            .map(|(.., bracket)| bracket.midpoint())
            .unwrap_or_default();

        Ok(Calibration {
            offset,
            bracket_width: Duration::from_secs_f64(bracket_width.max(0.0)),
            staleness,
            drift_ppm,
            drift_residual,
            suppression: suppression(&others),
            converge: converge(&reads, 8),
            advert_converge: advert_converge(&free_running, lower_best, 6),
            round_trips: reads.len(),
            span: Duration::from_secs_f64(
                self.samples
                    .iter()
                    .map(|s| s.host)
                    .fold(0.0, f64::max)
                    .max(0.0),
            ),
            blocks,
            contradictory,
            undecoded: self.undecoded,
            short_rtts: short,
            shortest_rtt: Duration::from_secs_f64(shortest.max(0.0)),
        })
    }
}

/// The bracket over one block, and whether it was possible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockReport {
    /// Where the block starts, from the calibrator's origin.
    pub at: Duration,
    /// Round trips in it. The minima are biased estimators of the floors and
    /// creep down as this grows, so a thin block reports a bracket that is too
    /// wide rather than too narrow — the safe direction.
    pub n: usize,
    /// `min(a) + min(b)`, in seconds. Negative is impossible and is what
    /// `contradictory` records.
    pub width: f64,
    /// The shortest round trip in the block, in seconds.
    pub shortest_rtt: f64,
    /// The middle of the block's bracket. Only used as a point for the drift
    /// fit, and meaningless in absolute terms.
    pub midpoint: f64,
    /// Whether the width came out negative, which proves at least one sample in
    /// the block was not a round trip.
    pub contradictory: bool,
}

/// How much later an advertisement lands when a link is up to the box.
///
/// Compared at **matched sample counts**, because a minimum over fewer samples
/// sits higher for that reason alone and would otherwise look like a slower
/// path. This is the check that caught the 6–10 ms error; it costs nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Suppression {
    /// The matched count: the smaller of the two populations.
    pub n: usize,
    /// The connected population's floor, measured from whichever of the two
    /// landed soonest.
    pub connected: Duration,
    /// The free-running population's floor, on the same reference.
    pub free_running: Duration,
}

impl Suppression {
    /// How much later a connected advertisement lands, in seconds, signed.
    ///
    /// Positive means suppression moves the floor and not merely how often it
    /// is sampled, so a constant calibrated inside a connection may **not** be
    /// applied to a passive clock. About +9.53 ms on the box this was built
    /// against, which is why nothing here calibrates from the connected
    /// population.
    pub fn penalty(&self) -> f64 {
        self.connected.as_secs_f64() - self.free_running.as_secs_f64()
    }
}

/// What a capture turned out to say. Produced by [`Calibrator::finish`].
///
/// The one number most callers want is [`offset`](Calibration::offset). The
/// rest is there so a hardware run can be read rather than trusted — printing
/// one with `{}` renders the whole report, caveats included.
#[derive(Debug, Clone)]
pub struct Calibration {
    offset: Duration,
    bracket_width: Duration,
    staleness: Vec<(Stream, usize, Bracket)>,
    drift_ppm: Option<f64>,
    drift_residual: Option<f64>,
    suppression: Option<Suppression>,
    converge: Vec<(usize, f64)>,
    advert_converge: Vec<(usize, f64)>,
    round_trips: usize,
    span: Duration,
    blocks: Vec<BlockReport>,
    contradictory: usize,
    undecoded: usize,
    short_rtts: usize,
    shortest_rtt: Duration,
}

impl Calibration {
    /// The constant to hand [`FreeRun::jam`](crate::freerun::FreeRun::jam).
    ///
    /// The midpoint of the free-running advertisement's staleness bracket — how
    /// far behind the device's own clock the least delayed advertisement was
    /// when it landed. Infallible: [`Calibrator::finish`] refuses a capture
    /// without free-running advertisements, so there is always one to take.
    ///
    /// It is a midpoint and not a measurement. Applying it does not remove the
    /// error, it halves the worst case — from the bracket's width to half of
    /// it.
    pub fn offset(&self) -> Duration {
        self.offset
    }

    /// The width of the bracket on `θ`, pooled over the round trips and
    /// de-trended.
    ///
    /// This is the uncertainty floor of the whole exercise, and the quantity
    /// the 30 ms connection interval puts a floor under: a request waits for
    /// the next connection anchor and a response does not, so the two path
    /// floors this adds cannot both be small. macOS gives no way to negotiate a
    /// shorter interval, so on this host it is what it is.
    ///
    /// Note this is wider than the advertisement's staleness bracket, and not
    /// by accident: this pools `min(b)` over the *reads*, while a staleness
    /// bracket takes its lower bound from whichever stream reached the host
    /// soonest, which is the advertisements.
    pub fn bracket_width(&self) -> Duration {
        self.bracket_width
    }

    /// The staleness of a stream's least delayed reading, if the capture held
    /// any of that stream.
    ///
    /// Two consequences of the arithmetic are worth stating rather than
    /// discovering. The lower bound comes from whichever stream reached the
    /// host soonest after being stamped, so **that stream reads zero by
    /// construction and the content is the upper bound**. And every bracket has
    /// the same width, so `high` is `low` plus that width and both ends are
    /// non-negative by construction rather than by clamping.
    pub fn staleness(&self, stream: Stream) -> Option<Bracket> {
        self.staleness
            .iter()
            .find(|(s, ..)| *s == stream)
            .map(|(.., bracket)| *bracket)
    }

    /// How many readings of a stream the capture held.
    pub fn count(&self, stream: Stream) -> usize {
        self.staleness
            .iter()
            .find(|(s, ..)| *s == stream)
            .map(|(_, n, _)| *n)
            .unwrap_or(0)
    }

    /// How fast the device's clock runs against this host's, from the block
    /// midpoints.
    ///
    /// `None` until three usable blocks exist — two points define a line and
    /// say nothing about whether it is one. This is the same quantity
    /// [`freerun::Drift`](crate::freerun::Drift) measures from one-way anchors,
    /// arrived at from round trips instead: if the two disagree, one of them is
    /// wrong.
    pub fn drift_ppm(&self) -> Option<f64> {
        self.drift_ppm
    }

    /// The connected-against-free-running comparison, at matched sample counts.
    ///
    /// `None` when the capture held no connected advertisements to compare
    /// against — which is not a problem, since the free-running population is
    /// the one a calibration is built from.
    pub fn suppression(&self) -> Option<Suppression> {
        self.suppression
    }

    /// Whether the advertisement floor has stopped moving.
    ///
    /// A minimum is a biased estimator of a floor and creeps downwards as
    /// samples accumulate, so a short capture reports a floor that is too
    /// *high*. For the bracket that is the safe direction — it over-reports the
    /// uncertainty — but for a staleness figure it is the unsafe one: still
    /// falling means the true staleness is *lower* than reported.
    ///
    /// The rule: the last step of the advertisement convergence moved the floor
    /// by less than [`SETTLED_FRACTION`] of [`bracket_width`](Self::bracket_width).
    /// That fraction is a judgement and not a measurement. `false` does not
    /// invalidate the figure; it means quote it with the caveat attached.
    pub fn settled(&self) -> bool {
        let mut steps = self.advert_converge.iter().rev();
        let (Some((_, last)), Some((_, previous))) = (steps.next(), steps.next()) else {
            return false;
        };
        (last - previous).abs() < SETTLED_FRACTION * self.bracket_width.as_secs_f64()
    }

    /// Round trips that survived the checks and went into the answer.
    pub fn round_trips(&self) -> usize {
        self.round_trips
    }

    /// How long the capture ran.
    pub fn span(&self) -> Duration {
        self.span
    }

    /// Blocks whose bracket came out negative and were dropped, along with
    /// their reads.
    ///
    /// Worth reporting rather than hiding: this check has caught two real
    /// samples, and a capture where it fires often is a capture to distrust
    /// even though the surviving blocks reduce cleanly.
    pub fn contradictory_blocks(&self) -> usize {
        self.contradictory
    }

    /// The per-block brackets, in time order.
    pub fn blocks(&self) -> &[BlockReport] {
        &self.blocks
    }
}

impl fmt::Display for Calibration {
    /// The whole report, with the caveats attached to the numbers rather than
    /// left in a doc nobody opens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} round trips over {:.0} s",
            self.round_trips,
            self.span.as_secs_f64()
        )?;
        if self.undecoded > 0 {
            writeln!(f, "  {} payload(s) did not decode", self.undecoded)?;
        }
        writeln!(f)?;

        writeln!(f, "# Are these round trips")?;
        writeln!(f)?;
        writeln!(
            f,
            "  shortest {:.3} ms; {} of {} under one connection interval, which is",
            self.shortest_rtt.as_secs_f64() * 1e3,
            self.short_rtts,
            self.round_trips
        )?;
        writeln!(f, "  the least a real one can take.")?;
        writeln!(f)?;

        writeln!(f, "# The bracket, per block")?;
        writeln!(f)?;
        writeln!(f, "   at s      n     width    min rtt")?;
        for block in &self.blocks {
            writeln!(
                f,
                "  {:6.0} {:6} {:9.3} {:10.3}{}",
                block.at.as_secs_f64(),
                block.n,
                block.width * 1e3,
                block.shortest_rtt * 1e3,
                if block.contradictory {
                    "  contradictory"
                } else {
                    ""
                }
            )?;
        }
        writeln!(f)?;
        if self.contradictory > 0 {
            writeln!(
                f,
                "  {} block(s) came out with a negative width, which is impossible for a",
                self.contradictory
            )?;
            writeln!(
                f,
                "  real round trip. They and their reads are excluded from everything"
            )?;
            writeln!(f, "  above, since a pooled minimum would inherit the same")?;
            writeln!(f, "  sample.")?;
            writeln!(f)?;
        }
        match (self.drift_ppm, self.drift_residual) {
            (Some(ppm), Some(rms)) => {
                writeln!(
                    f,
                    "  the midpoints move at {ppm:+.1} ppm, scattering {:.3} ms rms about",
                    rms * 1e3
                )?;
                writeln!(
                    f,
                    "  that line (Theil-Sen, so one bad block cannot own the fit)."
                )?;
            }
            _ => {
                writeln!(
                    f,
                    "  too few usable blocks to call a drift rate; use a longer capture."
                )?;
            }
        }
        writeln!(f)?;

        writeln!(f, "# Does it converge")?;
        writeln!(f)?;
        writeln!(
            f,
            "  A minimum creeps down as samples accumulate, so a short run reports a"
        )?;
        writeln!(
            f,
            "  bracket that is too wide — safe, but only worth quoting once it has"
        )?;
        writeln!(f, "  stopped moving.")?;
        writeln!(f)?;
        writeln!(f, "       n     width")?;
        for (n, width) in &self.converge {
            writeln!(f, "  {n:6} {:9.3}", width * 1e3)?;
        }
        writeln!(f)?;

        writeln!(f, "# What a one-way reading costs")?;
        writeln!(f)?;
        writeln!(
            f,
            "  How far behind the device's own clock each stream's *least delayed*"
        )?;
        writeln!(
            f,
            "  reading was when it landed — the error left in a clock that anchors on"
        )?;
        writeln!(f, "  the best reading it sees.")?;
        writeln!(f)?;
        for (stream, n, bracket) in &self.staleness {
            writeln!(f, "  {:>22} {n:7}    {bracket}", stream.label())?;
        }
        writeln!(f)?;
        writeln!(
            f,
            "  Low ends are against whichever stream reached the host soonest, so that"
        )?;
        writeln!(
            f,
            "  stream reads zero by construction and the content is the upper bound."
        )?;
        writeln!(f)?;

        if let Some(s) = self.suppression {
            writeln!(f, "# Connected against free-running, at matched n")?;
            writeln!(f)?;
            writeln!(
                f,
                "    connected    n={:4}   floor {:8.3} ms",
                s.n,
                s.connected.as_secs_f64() * 1e3
            )?;
            writeln!(
                f,
                "    free-running n={:4}   floor {:8.3} ms",
                s.n,
                s.free_running.as_secs_f64() * 1e3
            )?;
            writeln!(f, "    penalty                {:+8.3} ms", s.penalty() * 1e3)?;
            writeln!(f)?;
            writeln!(
                f,
                "  Matched counts, because a minimum over fewer samples sits higher for"
            )?;
            writeln!(
                f,
                "  that reason alone and would otherwise look like a slower path."
            )?;
            writeln!(f)?;
        }

        writeln!(f, "# Has the advertisement floor settled")?;
        writeln!(f)?;
        for (n, floor) in &self.advert_converge {
            writeln!(f, "    n={n:5}   floor {:8.3} ms", floor * 1e3)?;
        }
        writeln!(f)?;
        match self.settled() {
            true => writeln!(f, "  Settled: the last step moved it by under {SETTLED_FRACTION} of the bracket.")?,
            false => {
                writeln!(
                    f,
                    "  NOT settled. Still falling means the true staleness is *lower* than"
                )?;
                writeln!(
                    f,
                    "  reported — the opposite of the direction the bracket errs in, so say"
                )?;
                writeln!(f, "  so rather than quoting the number flat.")?;
            }
        }
        writeln!(f)?;
        writeln!(
            f,
            "offset to apply {:.3} ms, bracket {:.3} ms",
            self.offset.as_secs_f64() * 1e3,
            self.bracket_width.as_secs_f64() * 1e3
        )?;
        writeln!(
            f,
            "This is a path constant, calibrated once. It does not decay, and it is"
        )?;
        writeln!(
            f,
            "not a time transfer — the time still comes from the advertisements."
        )
    }
}

/// The device's stamp in seconds since its own midnight.
///
/// [`Timecode::frame_position`] already carries the sub-frame fraction, so
/// dividing by the nominal rate gives whole seconds plus the sub-frame trailer
/// and nothing has to be added back on. The nominal rate is the right divisor
/// because it is what `frame_position` counts in.
fn device_seconds(tc: &Timecode) -> f64 {
    tc.frame_position() / tc.rate.fps.max(1) as f64
}

/// The smallest of an iterator of finite values, or infinity if it is empty.
fn min_of(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(f64::INFINITY, f64::min)
}

/// Theil-Sen slope and intercept of `(x, y)` pairs, or `None` if degenerate.
///
/// The median of all pairwise slopes, not least squares. These are block
/// bracket midpoints — few of them, and one bad block moves its midpoint by
/// tens of milliseconds where the honest ones sit within a fraction of one.
/// Least squares hands such a block most of the fit: on a 314 s capture a
/// single bad block turned +8.8 ppm into −23.3 ppm at 3.8 ms rms, and since the
/// slope then de-trends everything pooled, the wrong slope quietly turned the
/// bracket negative rather than failing.
///
/// Theil-Sen ignores it. It tolerates up to 29% of the points being arbitrary,
/// needs no threshold to be chosen, and on clean input agrees with least
/// squares to well inside what any of this resolves.
fn fit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let mut slopes = Vec::new();
    for (i, &(xi, yi)) in points.iter().enumerate() {
        for &(xj, yj) in &points[i + 1..] {
            if xj != xi {
                slopes.push((yj - yi) / (xj - xi));
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(f64::total_cmp);
    let slope = slopes[slopes.len() / 2];
    // The intercept that puts the line through the median residual, which is
    // the matching robust choice — a mean here would let the outlier back in.
    let mut offsets: Vec<f64> = points.iter().map(|(x, y)| y - slope * x).collect();
    offsets.sort_by(f64::total_cmp);
    Some((slope, offsets[offsets.len() / 2]))
}

/// The pooled bracket over growing prefixes of the reads.
fn converge(reads: &[Sample], steps: usize) -> Vec<(usize, f64)> {
    let mut ordered = reads.to_vec();
    ordered.sort_by(|x, y| x.host.total_cmp(&y.host));
    let step = (ordered.len() / steps).max(1);
    let mut out = Vec::new();
    let mut cut = step;
    while cut <= ordered.len() {
        let window = &ordered[..cut];
        let upper = min_of(window.iter().map(|s| s.a_flat));
        out.push((cut, upper + min_of(window.iter().map(|s| s.b_flat))));
        cut += step;
    }
    out
}

/// The free-running advertisement floor over growing prefixes.
///
/// The same reasoning as [`converge`], but the direction that matters is the
/// opposite one: a floor still falling at the last step means the true
/// staleness is *lower* than reported.
fn advert_converge(adverts: &[Sample], lower_best: f64, steps: usize) -> Vec<(usize, f64)> {
    let mut ordered = adverts.to_vec();
    ordered.sort_by(|x, y| x.host.total_cmp(&y.host));
    let step = (ordered.len() / steps).max(1);
    let mut out = Vec::new();
    let mut cut = step;
    while cut <= ordered.len() {
        let floor = min_of(ordered[..cut].iter().map(|s| s.b_flat));
        out.push((cut, floor - lower_best));
        cut += step;
    }
    out
}

/// Connected against free-running advertisements, at matched sample counts.
///
/// Both floors are measured from whichever of the two populations landed
/// soonest, so one of them is zero by construction and the other is the
/// penalty. `None` unless both populations exist.
fn suppression(others: &[Sample]) -> Option<Suppression> {
    let take = |stream: Stream| {
        let mut rows: Vec<Sample> = others
            .iter()
            .copied()
            .filter(|s| s.stream == stream)
            .collect();
        rows.sort_by(|x, y| x.host.total_cmp(&y.host));
        rows
    };
    let inside = take(Stream::ConnectedAdvert);
    let between = take(Stream::Advert);
    if inside.is_empty() || between.is_empty() {
        return None;
    }
    // A minimum over fewer samples sits higher for that reason alone, so the
    // two populations are truncated to the same count before their floors are
    // compared. Without this a thinner sample masquerades as a slower path,
    // which is exactly how the first answer to this question came out wrong.
    let n = inside.len().min(between.len());
    let floor_in = min_of(inside[..n].iter().map(|s| s.b_flat));
    let floor_out = min_of(between[..n].iter().map(|s| s.b_flat));
    let reference = floor_in.min(floor_out);
    Some(Suppression {
        n,
        connected: Duration::from_secs_f64((floor_in - reference).max(0.0)),
        free_running: Duration::from_secs_f64((floor_out - reference).max(0.0)),
    })
}

/// Seconds from `earlier` to `later`, negative if they are the other way round.
///
/// Samples are stamped as they arrive and a capture can hand them over out of
/// order; [`Instant`] subtraction would saturate at zero and quietly bias the
/// result.
fn secs_between(later: Instant, earlier: Instant) -> f64 {
    match later.checked_duration_since(earlier) {
        Some(span) => span.as_secs_f64(),
        None => -earlier.duration_since(later).as_secs_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timecode::Rate;

    const FPS: u8 = 24;
    /// The connection interval the synthetic link runs at.
    const INTERVAL: f64 = 0.030;

    /// The floors of the box this is modelled on, from `PROTOCOL.md`: a request
    /// waits 1.7 ms beyond the anchor grid at best, a response 1.7 ms, and an
    /// advertisement — which waits for no anchor at all — 0.35 ms. Those three
    /// reproduce the measured capture to a tenth of a millisecond, which is
    /// most of the reason to believe the arithmetic here matches the script's.
    const OUT_FLOOR: f64 = 0.0017;
    const RET_FLOOR: f64 = 0.0017;
    const ADVERT_FLOOR: f64 = 0.00035;
    /// +8.6 ppm, as measured.
    const DRIFT: f64 = 8.6e-6;

    fn rate() -> Rate {
        Rate::whole(FPS)
    }

    fn stamp(device_secs: f64) -> Timecode {
        Timecode::at_frame_position(device_secs * FPS as f64, rate())
    }

    fn dur(secs: f64) -> Duration {
        Duration::from_secs_f64(secs.max(0.0))
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// A synthetic link with known floors, so a test can ask whether the
    /// estimator recovered what was put in rather than what it computed.
    struct Wire {
        /// The device clock minus the host's at the origin. Deliberately huge,
        /// as it is in life — a time of day against an interval — so that any
        /// arithmetic that fails to cancel it shows up enormous.
        theta0: f64,
        drift: f64,
        out_floor: f64,
        ret_floor: f64,
        advert_floor: f64,
    }

    impl Default for Wire {
        fn default() -> Wire {
            Wire {
                theta0: 10.0 * 3600.0,
                drift: DRIFT,
                out_floor: OUT_FLOOR,
                ret_floor: RET_FLOOR,
                advert_floor: ADVERT_FLOOR,
            }
        }
    }

    impl Wire {
        fn theta(&self, host: f64) -> f64 {
            self.theta0 + self.drift * host
        }

        /// One round trip whose request landed at phase `p` of the connection
        /// anchor grid.
        ///
        /// The request waits out the rest of the interval and the response,
        /// already at the device, waits not at all — so the round trip is a
        /// constant while the two legs slide against each other. That is why
        /// the two minima are achieved by *different* samples, and why the
        /// bracket comes out far narrower than the shortest round trip.
        fn round_trip(&self, cal: &mut Calibrator, base: Instant, t0: f64, p: f64) {
            let d_out = self.out_floor + (INTERVAL - p);
            let d_ret = self.ret_floor + p;
            let stamped = t0 + d_out;
            cal.round_trip(
                &stamp(stamped + self.theta(stamped)),
                base + dur(t0),
                base + dur(t0 + d_out + d_ret),
            );
        }

        fn advert(&self, cal: &mut Calibrator, base: Instant, arrived: f64, delay: f64, link: Link) {
            let stamped = arrived - delay;
            cal.advert(&stamp(stamped + self.theta(stamped)), base + dur(arrived), link);
        }
    }

    /// The phase of the `n`th read on the anchor grid, stepped by a prime so it
    /// sweeps rather than landing on one point of it — the same trick the
    /// capture tool's dither plays, and the reason both floors get sampled.
    fn phase(n: u64) -> f64 {
        ((n.wrapping_mul(7_919) % 31_000) as f64 / 1e6).min(INTERVAL)
    }

    /// A capture of `seconds`, at the rates a real one runs at: reads four a
    /// second while connected, advertisements about 1.4 a second.
    fn capture(wire: &Wire, seconds: f64) -> (Instant, Calibrator) {
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < seconds {
            wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
            n += 1;
        }
        let mut k = 0u64;
        while 0.6 + k as f64 * 0.7 < seconds {
            // Jitter that reaches zero every eleventh advertisement, so the
            // floor is genuinely achieved and not merely approached.
            let jitter = ((k * 37) % 11) as f64 * 0.0005;
            wire.advert(
                &mut cal,
                base,
                0.6 + k as f64 * 0.7,
                wire.advert_floor + jitter,
                Link::Down,
            );
            k += 1;
        }
        (base, cal)
    }

    #[test]
    fn recovers_the_floors_it_was_given() {
        let wire = Wire::default();
        let done = capture(&wire, 300.0).1.finish().expect("a clean capture");

        // min(d_out) + min(d_ret), which is what the bracket is.
        assert!(
            (ms(done.bracket_width()) - 3.4).abs() < 0.05,
            "bracket {:.3} ms, wanted the 3.4 ms put in",
            ms(done.bracket_width())
        );
        // And narrower than the shortest round trip, which is the whole point:
        // the two minima are achieved by different samples.
        let shortest = done
            .blocks()
            .iter()
            .map(|b| b.shortest_rtt)
            .fold(f64::INFINITY, f64::min);
        assert!(
            shortest > 0.033 && done.bracket_width().as_secs_f64() < shortest / 8.0,
            "shortest rtt {shortest:.4} s against bracket {:.4} s",
            done.bracket_width().as_secs_f64()
        );
    }

    #[test]
    fn the_advertisement_path_beats_the_read_path_and_reads_zero_by_construction() {
        let done = capture(&Wire::default(), 300.0).1.finish().unwrap();

        let advert = done.staleness(Stream::Advert).expect("adverts were fed in");
        let read = done.staleness(Stream::Read).expect("reads were fed in");

        // The advertisement stream gives the tightest lower bound on theta, so
        // it reads zero and everything else is measured against it.
        assert_eq!(advert.low, Duration::ZERO, "the best stream reads zero");
        assert!(
            (ms(read.low) - 1.35).abs() < 0.05,
            "read low {:.3} ms, wanted the 1.35 ms the floors differ by",
            ms(read.low)
        );
        // Every bracket is the same width — they differ only by where they
        // start — so the content of each is its upper end.
        assert_eq!(advert.width(), read.width());
        assert!(
            (ms(advert.high) - 2.05).abs() < 0.05,
            "advert staleness high {:.3} ms",
            ms(advert.high)
        );
        // The offset to apply is that bracket's midpoint, and nothing else.
        assert_eq!(done.offset(), advert.midpoint());
    }

    #[test]
    fn pooling_across_drift_without_detrending_is_spuriously_narrow() {
        let wire = Wire::default();
        let (base, cal) = capture(&wire, 300.0);
        let done = cal.finish().unwrap();

        // What the estimator says, having taken the fitted drift out.
        let detrended = ms(done.bracket_width());

        // What pooling the same samples raw would have said. `a` rises with a
        // positive drift while `b` falls, so min(a) is taken from the start of
        // the capture and min(b) from the end — the two bounds then constrain
        // *different* values of theta and the interval between them is not a
        // bound on anything.
        let mut raw_a = f64::INFINITY;
        let mut raw_b = f64::INFINITY;
        let mut probe = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < 300.0 {
            wire.round_trip(&mut probe, base, n as f64 * 0.25, phase(n));
            n += 1;
        }
        for sample in probe.samples.iter().filter(|s| s.stream == Stream::Read) {
            raw_a = raw_a.min(sample.a.unwrap());
            raw_b = raw_b.min(sample.b);
        }
        let raw = (raw_a + raw_b) * 1e3;

        assert!(
            (detrended - 3.4).abs() < 0.05,
            "de-trended {detrended:.3} ms, wanted 3.4"
        );
        // 8.6 ppm over 300 s is 2.58 ms of the 3.4, so raw reads about 0.8.
        assert!(
            raw < detrended - 2.0,
            "raw pooled {raw:.3} ms should be far narrower than the true {detrended:.3} ms — \
             if it isn't, this test is no longer exercising the drift it was built to"
        );
        assert!(raw > 0.0, "raw {raw:.3} ms");
    }

    #[test]
    fn a_block_whose_bracket_is_impossible_is_dropped_with_its_reads() {
        let wire = Wire::default();
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < 300.0 {
            wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
            n += 1;
        }
        // One sample in the fourth block whose device stamp predates the
        // request that returned it — the 88 ms round trip with a stamp 27 ms
        // out of place that a real capture held, at the moment a box died.
        let t0 = 100.0;
        cal.round_trip(
            &stamp(t0 - 0.027 + wire.theta(t0)),
            base + dur(t0),
            base + dur(t0 + 0.088),
        );
        let mut k = 0u64;
        while 0.6 + k as f64 * 0.7 < 300.0 {
            let jitter = ((k * 37) % 11) as f64 * 0.0005;
            wire.advert(
                &mut cal,
                base,
                0.6 + k as f64 * 0.7,
                wire.advert_floor + jitter,
                Link::Down,
            );
            k += 1;
        }

        // The impossible sample is a minimum on both legs at once, so had it
        // survived it would have become the answer wherever it pooled.
        let poisoned = cal
            .samples
            .iter()
            .filter(|s| s.stream == Stream::Read)
            .map(|s| s.a.unwrap())
            .fold(f64::INFINITY, f64::min)
            + cal
                .samples
                .iter()
                .filter(|s| s.stream == Stream::Read)
                .map(|s| s.b)
                .fold(f64::INFINITY, f64::min);
        assert!(
            poisoned < 0.0,
            "the injected sample should make the pooled bracket negative, got {poisoned:.6} s"
        );

        let done = cal.finish().expect("one bad block out of ten is survivable");
        assert_eq!(done.contradictory_blocks(), 1);
        assert!(
            (ms(done.bracket_width()) - 3.4).abs() < 0.05,
            "bracket {:.3} ms — the bad block's reads reached the answer",
            ms(done.bracket_width())
        );
        // Dropped, not merely flagged: the block's reads are gone from the count.
        assert!(
            done.round_trips() < 1201,
            "{} round trips, so the block's reads survived",
            done.round_trips()
        );
    }

    #[test]
    fn every_block_impossible_is_refused_rather_than_reduced() {
        let wire = Wire::default();
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < 90.0 {
            wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
            n += 1;
        }
        // One impossible sample in each of the three blocks. It takes both an
        // impossible sample and an honest one to make a block negative: the two
        // minima have to come from different samples, since a single sample's
        // `a + b` is just its round trip and cannot be below zero however
        // misplaced the stamp inside it is.
        for t0 in [10.0, 40.0, 70.0] {
            cal.round_trip(
                &stamp(t0 - 0.027 + wire.theta(t0)),
                base + dur(t0),
                base + dur(t0 + 0.088),
            );
        }
        wire.advert(&mut cal, base, 5.0, wire.advert_floor, Link::Down);

        match cal.finish() {
            Err(Flaw::EveryBlockContradictory { blocks }) => assert_eq!(blocks, 3),
            other => panic!("wanted EveryBlockContradictory, got {other:?}"),
        }
    }

    #[test]
    fn a_subscribed_capture_is_refused_because_its_reads_are_not_round_trips() {
        let wire = Wire::default();
        let base = Instant::now();

        // 83.6% came back under one interval in the real subscribed capture;
        // 5% is already far past what a real link can do.
        let mut cal = Calibrator::since(base);
        for n in 0..200u64 {
            let t0 = n as f64 * 0.25;
            match n % 20 == 0 {
                // Resolved by the notification stream: the "response" arrives
                // 31 µs after the request, which no Bluetooth round trip can.
                true => cal.round_trip(
                    &stamp(t0 + 0.00001 + wire.theta(t0)),
                    base + dur(t0),
                    base + dur(t0 + 0.000031),
                ),
                false => wire.round_trip(&mut cal, base, t0, phase(n)),
            }
        }
        wire.advert(&mut cal, base, 5.0, wire.advert_floor, Link::Down);

        match cal.finish() {
            Err(Flaw::NotRoundTrips {
                short,
                total,
                shortest,
            }) => {
                assert_eq!(short, 10);
                assert_eq!(total, 200);
                assert!(shortest < Duration::from_micros(100), "{shortest:?}");
            }
            other => panic!("wanted NotRoundTrips, got {other:?}"),
        }

        // One stray sample in two hundred is under the 2% bar and must not
        // throw the capture away — real captures do show the occasional oddity.
        let mut cal = Calibrator::since(base);
        for n in 0..200u64 {
            let t0 = n as f64 * 0.25;
            match n == 7 {
                true => cal.round_trip(
                    &stamp(t0 + 0.00001 + wire.theta(t0)),
                    base + dur(t0),
                    base + dur(t0 + 0.000031),
                ),
                false => wire.round_trip(&mut cal, base, t0, phase(n)),
            }
        }
        wire.advert(&mut cal, base, 5.0, wire.advert_floor, Link::Down);
        assert!(cal.finish().is_ok(), "1 of 200 is under the 2% bar");
    }

    #[test]
    fn a_device_clock_that_steps_is_refused() {
        let wire = Wire::default();
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        for n in 0..100u64 {
            let t0 = n as f64 * 0.25;
            // A re-jam two seconds forward, half way through.
            let jump = if n >= 50 { 2.0 } else { 0.0 };
            let d_out = wire.out_floor + INTERVAL;
            cal.round_trip(
                &stamp(t0 + d_out + wire.theta(t0 + d_out) + jump),
                base + dur(t0),
                base + dur(t0 + d_out + wire.ret_floor),
            );
        }
        match cal.finish() {
            Err(Flaw::ClockJumped { at, by }) => {
                assert!((by - 2.0).abs() < 0.01, "step {by}");
                assert!((at.as_secs_f64() - 12.5).abs() < 0.5, "at {at:?}");
            }
            other => panic!("wanted ClockJumped, got {other:?}"),
        }
    }

    #[test]
    fn a_capture_with_no_free_running_advertisements_is_refused() {
        let wire = Wire::default();
        let (base, mut cal) = {
            let base = Instant::now();
            let mut cal = Calibrator::since(base);
            let mut n = 0u64;
            while (n as f64) * 0.25 < 60.0 {
                wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
                n += 1;
            }
            (base, cal)
        };
        // Every advertisement caught while a link was up — which is exactly the
        // capture that produced the wrong first answer.
        for k in 0..80u64 {
            wire.advert(
                &mut cal,
                base,
                0.6 + k as f64 * 0.7,
                wire.advert_floor + 0.00953,
                Link::Up,
            );
        }
        assert!(matches!(cal.finish(), Err(Flaw::NoFreeRunning)));
    }

    #[test]
    fn suppression_is_measured_at_matched_n_so_a_thin_sample_is_not_a_slow_path() {
        let wire = Wire::default();
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < 300.0 {
            wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
            n += 1;
        }

        // Both populations are drawn from the *same* sequence of delivery
        // delays, so there is no real difference between these two paths at
        // all. The only difference is how many samples each got: 240 connected
        // against 24 free-running. Since the delays keep improving, a minimum
        // over ten times the samples sits lower for that reason alone — and an
        // unmatched comparison reads that as the connected path being faster,
        // which is both false and backwards.
        let delay = |k: u64| wire.advert_floor + 0.004 - k as f64 * 0.000015;
        for k in 0..240u64 {
            wire.advert(&mut cal, base, 1.0 + k as f64 * 1.2, delay(k), Link::Up);
        }
        for k in 0..24u64 {
            wire.advert(&mut cal, base, 1.4 + k as f64 * 1.2, delay(k), Link::Down);
        }

        let done = cal.finish().unwrap();
        let s = done.suppression().expect("both populations present");
        assert_eq!(s.n, 24, "matched to the smaller population");

        // Truncated to the same count the two see identical delays, so the
        // measured penalty is zero — which is the truth about these two paths.
        assert!(
            s.penalty().abs() < 0.0005,
            "penalty {:.3} ms — populations drawn from one sequence must read zero",
            s.penalty() * 1e3
        );

        // The artefact matching exists to remove, read off the full-population
        // floors: unmatched, the thin population looks milliseconds slower on
        // sample count alone.
        let floor_of = |stream: Stream| done.staleness(stream).unwrap().low.as_secs_f64();
        let artefact = floor_of(Stream::Advert) - floor_of(Stream::ConnectedAdvert);
        assert!(
            artefact > 0.003,
            "unmatched, the thin population should look {:.3} ms slower — if it does not, \
             this test no longer exercises what matching is for",
            artefact * 1e3
        );
    }

    #[test]
    fn theil_sen_survives_a_bad_block_where_least_squares_does_not() {
        // Ten honest block midpoints on a +8.6 ppm line, and one block 35 ms
        // wide — the shape that turned +8.8 ppm into -23.3 ppm on a real
        // capture, and then de-trended everything pooled with the wrong slope.
        let mut points: Vec<(f64, f64)> = (0..10)
            .map(|i| {
                let x = i as f64 * 30.0 + 15.0;
                (x, DRIFT * x + ((i % 3) as f64 - 1.0) * 1e-5)
            })
            .collect();
        points.push((315.0, DRIFT * 315.0 - 0.035));

        let (slope, _) = fit(&points).expect("eleven points");

        let n = points.len() as f64;
        let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
        let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
        let least_squares = points
            .iter()
            .map(|(x, y)| (x - mean_x) * (y - mean_y))
            .sum::<f64>()
            / points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum::<f64>();

        assert!(
            (slope - DRIFT).abs() < 1e-6,
            "Theil-Sen gave {:+.1} ppm, wanted {:+.1}",
            slope * 1e6,
            DRIFT * 1e6
        );
        assert!(
            (least_squares - DRIFT).abs() > 1e-5,
            "least squares gave {:+.1} ppm — if it survives this block too, the \
             block is no longer bad enough for this test to mean anything",
            least_squares * 1e6
        );
        // And the sign flip that made it dangerous rather than merely wrong.
        assert!(least_squares < 0.0, "{:+.1} ppm", least_squares * 1e6);
    }

    #[test]
    fn a_floor_still_falling_is_reported_unsettled() {
        let wire = Wire::default();

        // Advertisement delays that keep improving to the last sample, so the
        // minimum is still coming down when the capture ends.
        let base = Instant::now();
        let mut cal = Calibrator::since(base);
        let mut n = 0u64;
        while (n as f64) * 0.25 < 300.0 {
            wire.round_trip(&mut cal, base, n as f64 * 0.25, phase(n));
            n += 1;
        }
        for k in 0..120u64 {
            let improving = 0.004 - k as f64 * 0.00003;
            wire.advert(&mut cal, base, 0.6 + k as f64 * 2.0, improving, Link::Down);
        }
        let falling = cal.finish().unwrap();
        assert!(
            !falling.settled(),
            "a floor improving every sample has not been found: {:?}",
            falling.advert_converge
        );

        // The same capture with a floor that is genuinely reached early.
        assert!(
            capture(&wire, 300.0).1.finish().unwrap().settled(),
            "a floor hit every eleventh advertisement has settled"
        );
    }

    #[test]
    fn the_report_carries_the_caveats_and_never_the_raw_offset() {
        let done = capture(&Wire::default(), 300.0).1.finish().unwrap();
        let report = done.to_string();

        for wanted in [
            "path constant",
            "does not decay",
            "zero by construction",
            "advert (free-running)",
        ] {
            assert!(report.contains(wanted), "report is missing {wanted:?}");
        }

        // theta is a device time of day minus a host interval — 36,000-odd
        // seconds here — and means nothing on its own. Nothing may print it.
        assert!(
            !report.contains("36000") && !report.contains("3600000"),
            "the report leaked the absolute offset:\n{report}"
        );
    }

    #[test]
    fn an_empty_capture_says_which_thing_was_missing() {
        assert!(matches!(
            Calibrator::new().finish(),
            Err(Flaw::NoRoundTrips)
        ));
        // And the messages are worth reading, since they name the fix.
        assert!(Flaw::NoFreeRunning.to_string().contains("Rest between"));
        assert!(
            Flaw::NotRoundTrips {
                short: 2833,
                total: 3388,
                shortest: Duration::from_micros(31),
            }
            .to_string()
            .contains("subscribed")
        );
    }

    #[test]
    fn a_bracket_midpoint_halves_the_worst_case_and_never_goes_negative() {
        let bracket = Bracket {
            low: Duration::from_micros(0),
            high: Duration::from_micros(2090),
        };
        assert_eq!(bracket.width(), Duration::from_micros(2090));
        assert_eq!(bracket.midpoint(), Duration::from_micros(1045));

        // The worst case is the far end of the bracket; applying the midpoint
        // leaves half of it either way.
        let worst_before = bracket.high;
        let worst_after = bracket.width() / 2;
        assert!(worst_after * 2 == worst_before);
    }
}
