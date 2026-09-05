//! Taking the round trips the [estimator][crate::jam] reduces.
//!
//! That module is arithmetic over samples somebody else collected. This is the
//! somebody else, for the one sample kind that needs a connection: an
//! ATT read on `0dab144c`, stamped either side, which is a round trip where an
//! advertisement structurally cannot be. Everything else a calibration wants —
//! the free-running advertisements — arrives on a passive scan the caller is
//! already running, so this hands the caller a [`Pass`] to feed them into and
//! then stays out of the way.
//!
//! ```no_run
//! use std::time::Instant;
//!
//! use shokushu::ble::jam::Calibrate;
//! use shokushu::ble::{Event, Scanner};
//!
//! # async fn run() -> shokushu::Result<()> {
//! // One connection, then the box is left alone for good.
//! let mut pass = Calibrate::new().name("ricki").run().await?;
//! let target = pass.device().clone();
//!
//! let mut scan = Scanner::start().await?;
//! while let Some(event) = scan.next().await {
//!     // Only this box's advertisements: another Tentacle is a second unknown
//!     // offset, not a second reading of this one.
//!     if let Event::Timecode { id, timecode, at } = &event
//!         && *id == target
//!     {
//!         pass.advert(timecode, *at);
//!     }
//!     if !pass.open(Instant::now()) {
//!         break;
//!     }
//! }
//! println!("{}", pass.finish()?);
//! # Ok(())
//! # }
//! ```
//!
//! # One connection, and why that is enough
//!
//! An earlier design ran a dozen connect/rest cycles. It did not need to.
//!
//! One 6.6 s connection — which is all this box gives, see `PROTOCOL.md` —
//! yields on the order of a hundred round trips, and that brackets `θ`
//! perfectly well. The bound is a *minimum* over the samples, and a minimum
//! over fewer samples sits higher, so a short capture reports a bracket that is
//! too **wide**. That is the safe direction: it over-states the uncertainty
//! rather than inventing precision.
//!
//! What a loop bought was two diagnostics, and neither is needed to apply a
//! constant. [`Calibration::suppression`](crate::jam::Calibration::suppression)
//! wants advertisements caught while a link is up, and only about four and a
//! half of those arrive per connection, so a matched comparison needs a dozen
//! connections. And [`Calibration::drift_ppm`](crate::jam::Calibration::drift_ppm)
//! is fitted from block midpoints, which wants three blocks of round trips and
//! so at least three well separated connections. Both are worth having and
//! `shokushu-gatt --scan --phase --reconnect` still collects them. Neither
//! changes the offset.
//!
//! Against that, every connection is a connection: the box stops answering
//! after a few dozen rapid ones, and whether being connected to can knock a box
//! off a timeline it shares with others is an open question in `PROTOCOL.md`.
//! Twelve connections to buy two diagnostics is a bad trade, so this takes one.
//!
//! Measured, one connection against fifteen on the same hardware: a bracket of
//! 3.553 ms against 3.11 ms, and an advertisement staleness of 0 to 2.276 ms
//! against 0 to 2.09 ms. Wider on both, which is the direction it has to err in.
//!
//! ## One connection is *just* enough, and the report says when it wasn't
//!
//! The bracket only becomes narrow once `dither` has swept `t0` far enough
//! around the connection anchor grid that its two minima come from different
//! samples. On that run it took nearly the whole connection: the pooled width
//! over growing prefixes of the 90 round trips sat at 33 ms through the first
//! 77 of them and then collapsed to 3.7 ms by 88.
//!
//! So a connection cut short reports a bracket near one connection interval,
//! and **that is a sweep that did not finish rather than a slow link**. It costs
//! nothing to check: a
//! [`bracket_width`](crate::jam::Calibration::bracket_width) anywhere near
//! [`CONNECTION_INTERVAL`](crate::jam::CONNECTION_INTERVAL) means run it again
//! rather than believe it. The figure is not wrong — the offset it yields is
//! still a true bound — merely too loose to be worth applying.
//!
//! # The drift this cannot take out, and what that costs
//!
//! Here is the one real wrinkle in the single-connection shape, stated plainly
//! because it is a real error and not a rounding one.
//!
//! `θ` is bracketed inside a 6.6 s window at the start. The advertisements that
//! give the other half of the answer keep landing for as long as the caller
//! feeds them — and the two clocks are drifting apart the whole time, at about
//! 8.6 ppm on the boxes this was measured against. Over 300 s that is 2.58 ms,
//! which is comparable to the entire 3.4 ms bracket. With one connection there
//! is only one block of round trips, so
//! [`Calibrator::finish`](crate::jam::Calibrator::finish) has a single midpoint,
//! fits no slope, and de-trends nothing. Nothing is wrong with that inside the
//! connection — 6.6 s of drift is 0.06 ms — but it does not reach the
//! advertisements.
//!
//! Which way it goes: `a` holds `θ` as it was during the connection and `b`
//! holds `-θ` whenever the advertisement landed, so a *later* advertisement has
//! a *smaller* `b` and wins the floor for a reason that is not delivery. The
//! advertisement floor is depressed by the drift accumulated over the window,
//! so the staleness — and with it
//! [`Calibration::offset`](crate::jam::Calibration::offset) — comes out too
//! **small**. It under-corrects; it does not over-correct.
//!
//! Two ways out were available. De-trend on the drift
//! [`freerun::Drift`](crate::freerun::Drift) already measures from the same
//! advertisements, or bound the window they are pooled over. **This bounds the
//! window**, at [`ADVERT_WINDOW`], for two reasons. The drift estimate from
//! one-way anchors is itself only good to a few ppm early in a run — the
//! `shokushu-ble --drift` column exists to say so — and a few ppm over five
//! minutes is a correction as large as the error it is removing. And bounding
//! the window needs nothing from [`crate::jam`] beyond what is there:
//! samples this does not feed in cannot bias anything.
//!
//! **What it costs, in numbers.** The quantity the drift eats into is not the
//! 3.4 ms round-trip bracket but the 2.05 ms the two relevant floors leave —
//! 1.7 ms on the outbound leg of a read, 0.35 ms on an advertisement — because
//! that sum is what the advertisement's staleness bracket spans. Drift takes
//! `8.6 ppm × window` off it, and half of that off the midpoint. At the 60 s
//! default that is 0.52 ms off the bracket and **0.26 ms off the offset**, on a
//! true offset of about 1.02 ms. So the correction is applied about a quarter
//! short: the worst case improves from 2.05 ms to about 1.29 ms where a
//! perfectly de-trended one would reach 1.02. [`Calibrate::window`] narrows it
//! — 30 s halves the bias — at the price of a floor taken over half as many
//! readings, which pushes the other way.
//!
//! **And there is a second line of defence, which is not this module's.** Push
//! the window past about four minutes and the drift exceeds the whole 2.05 ms.
//! The pooled bounds on `θ` then cross, and
//! [`Calibrator::finish`](crate::jam::Calibrator::finish) returns
//! [`Flaw::StreamsContradict`](crate::jam::Flaw::StreamsContradict) rather than
//! a plausible number near zero. A badly widened window fails loudly.
//!
//! None of this is worth chasing at this scale — the whole correction is 0.025
//! of a frame at 24 fps — but none of it is hidden either:
//! [`Calibration::settled`](crate::jam::Calibration::settled) reports whether
//! the floor stopped moving inside the window, and a run where it did not
//! should quote the number with that attached.
//!
//! # Never subscribe
//!
//! Nothing here subscribes and there is no way to ask it to. On macOS a Read
//! Response and a notification arrive through the same CoreBluetooth delegate
//! callback, so a read taken while subscribed is resolved by whichever lands
//! first: 83.6% of one subscribed capture came back inside a single connection
//! interval, the shortest in 31 µs. The values are genuine and the timing is
//! fiction. [`Flaw::NotRoundTrips`](crate::jam::Flaw::NotRoundTrips) refuses
//! such a capture, but the better fix is not to be able to take one.

use std::time::{Duration, Instant};

use btleplug::api::{
    bleuuid::uuid_from_u16, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use uuid::Uuid;

use crate::ble::{self, Advert};
use crate::error::{Error, Result};
use crate::jam::{self, Link};
use crate::Timecode;

/// The vendor characteristic carrying the timecode, headerless.
///
/// The only one of the four with a clock in it, and so the only one a round
/// trip is worth taking on: `0dab1280` changes when the box is synced and never
/// otherwise, and `0dab2496` has never changed at all. Reading either would
/// spend a connection anchor point to learn what the last read already said.
const TIMECODE_CHAR: Uuid = Uuid::from_u128(0x0dab_144c_2cb9_11e6_b67b_9e71_128c_ae77);

/// The record type an advertised timecode carries in its first header byte.
///
/// The vendor characteristic sends the same record with the header taken off,
/// so putting one back on is the whole of the decode — see [`vendor_timecode`].
const KIND_TIMECODE: u8 = 0x22;

/// How long to wait for a connect before giving up.
///
/// A box that has stopped answering connections — which this one does after a
/// few dozen rapid ones — otherwise hangs the whole calibration forever with
/// nothing to say why.
pub const CONNECT_GIVE_UP: Duration = Duration::from_secs(15);

/// How long to hold the link before hanging up, if the box hasn't already.
///
/// It normally has: `PROTOCOL.md` measures the connection at about 6.6 s
/// whatever the client does. This is the bound for a box that behaves
/// differently, and it is deliberately close to that measured life — holding a
/// link longer buys round trips this does not need, at the cost of keeping a
/// box in service off the air.
pub const SESSION_CAP: Duration = Duration::from_secs(12);

/// How long a single read may take before the link counts as gone.
///
/// The shortest real round trip is one 30 ms connection interval, so anything
/// approaching this is not slow, it is a link the device has already dropped
/// without saying so. [`SESSION_CAP`] is only tested between reads, so without
/// this a wedged read would never end.
const READ_GIVE_UP: Duration = Duration::from_secs(5);

/// How long after the connection advertisements are still pooled.
///
/// The window the module docs describe: past here the drift between the two
/// clocks has moved `θ` by enough to matter and there is no slope to take it
/// out with. 8.6 ppm over 60 s is 0.52 ms against a 3.4 ms bracket, and 60 s of
/// free-running advertisements is on the order of a hundred readings — more
/// than the 67 a side that gave `PROTOCOL.md` its matched comparison.
pub const ADVERT_WINDOW: Duration = Duration::from_secs(60);

/// How long to look for a box before giving up on there being one.
pub const SCAN_FOR: Duration = Duration::from_secs(12);

/// The block length handed to the [`Calibrator`](crate::jam::Calibrator).
///
/// Deliberately longer than any pass, so every round trip lands in one block.
/// This is load-bearing rather than tidy. With one connection's reads split
/// across two blocks by a boundary they happened to straddle,
/// [`Calibrator::finish`](crate::jam::Calibrator::finish) would have two
/// midpoints, fit a line through them, and de-trend everything by a slope
/// derived from six seconds of data — and a wrong slope does not fail, it
/// quietly moves the answer. One block fits no line at all, which is the
/// correct thing to do with 0.06 ms of drift.
const ONE_BLOCK: Duration = Duration::from_secs(3600);

/// Configures a single calibration pass.
///
/// The defaults are the ones to use; every setter here exists for a box that
/// behaves differently from the ones this was measured against.
#[derive(Debug, Clone)]
pub struct Calibrate {
    name: Option<String>,
    scan_for: Duration,
    window: Duration,
}

impl Default for Calibrate {
    fn default() -> Calibrate {
        Calibrate::new()
    }
}

impl Calibrate {
    pub fn new() -> Calibrate {
        Calibrate {
            name: None,
            scan_for: SCAN_FOR,
            window: ADVERT_WINDOW,
        }
    }

    /// Only calibrate against a box whose name contains `want`,
    /// case-insensitively — the same rule
    /// [`scan::Builder::name`](crate::ble::scan::Builder::name) applies.
    ///
    /// Worth setting whenever more than one box is in range. Without it this
    /// takes the first Tentacle it hears from, which is whichever one happened
    /// to advertise first and not a choice anybody made.
    pub fn name(mut self, want: impl Into<String>) -> Calibrate {
        self.name = Some(want.into());
        self
    }

    /// How long to look for a box before giving up. See [`SCAN_FOR`].
    pub fn scan_for(mut self, d: Duration) -> Calibrate {
        self.scan_for = d;
        self
    }

    /// How long after the connection advertisements are still pooled.
    ///
    /// See [`ADVERT_WINDOW`] for what widening it costs, which is drift that
    /// nothing here can take out, and what narrowing it costs, which is a floor
    /// measured over fewer readings.
    pub fn window(mut self, d: Duration) -> Calibrate {
        self.window = d;
        self
    }

    /// Takes the first adapter the platform offers, finds a box, and connects
    /// to it once.
    pub async fn run(self) -> Result<Pass> {
        let manager = Manager::new().await?;
        let central = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NoAdapter)?;
        self.run_on(central).await
    }

    /// The same, on an adapter you've already chosen.
    ///
    /// This scans to find the box and stops scanning before it connects —
    /// holding a scan up through a connect drops the link during service
    /// discovery every time on macOS. It leaves the adapter **not** scanning,
    /// so a caller that wants its own scan back should start it after this
    /// returns; that is also the moment [`Pass`]'s advertisement window opens.
    pub async fn run_on(self, central: Adapter) -> Result<Pass> {
        let mut events = central.events().await?;
        central.start_scan(ScanFilter::default()).await?;
        let found = self.look_for(&central, &mut events).await;
        // Stopped whether or not anything was found: leaving an adapter
        // scanning after a failure is a state the caller did not ask for.
        let _ = central.stop_scan().await;
        let (id, name) = found?;

        let mut peripheral = central.peripheral(&id).await?;
        // The origin sits here rather than at the top of the pass so that
        // `span` reports the measurement and not the hunt for a box to make it
        // on. Only that every sample shares one origin matters.
        let mut calibrator = jam::Calibrator::new().with_block(ONE_BLOCK);
        let opened = match connect(&peripheral).await {
            Ok(()) => Instant::now(),
            // CoreBluetooth forgets a peripheral that isn't being scanned for,
            // so the handle from the discovery scan can already be stale and
            // reconnecting through it fails with "Device not found". One
            // rediscovery, then one more attempt — this is a retry against a
            // known platform quirk, not a reconnect loop.
            Err(first) => {
                peripheral = rediscover(&central, &id).await?;
                match connect(&peripheral).await {
                    Ok(()) => Instant::now(),
                    Err(_) => {
                        return Err(Error::Connect {
                            name: name.clone(),
                            reason: first,
                        })
                    }
                }
            }
        };

        let session = converse(&peripheral, &mut calibrator).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), peripheral.disconnect()).await;
        let connected_for = opened.elapsed();
        // A discovery failure or a refused connection is worth an error; a link
        // that died early is not, because what it managed is still a capture.
        // `finish` is the thing that decides whether it was enough.
        let round_trips = session?;

        Ok(Pass {
            device: id,
            name,
            round_trips,
            connected_for,
            // Opened at the disconnect and not at the connect: an
            // advertisement caught while a link was up lands about 9.5 ms
            // later against the box's own stamp, and this pass has no way to
            // record one as such because it never scans while connected.
            adverts: Adverts::new(calibrator, Instant::now(), self.window),
        })
    }

    /// Scans until a Tentacle answering the name filter turns up.
    async fn look_for(
        &self,
        central: &Adapter,
        events: &mut (impl futures::Stream<Item = CentralEvent> + Unpin),
    ) -> Result<(PeripheralId, String)> {
        // By service data rather than by name: the name is whatever the owner
        // typed into the app, and a box that has never been named still
        // advertises 0xFDAC.
        let service = uuid_from_u16(ble::SERVICE_UUID_16);
        let want = self.name.as_ref().map(|n| n.to_lowercase());
        let deadline = tokio::time::sleep(self.scan_for);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = events.next() => {
                    let Some(CentralEvent::ServiceDataAdvertisement { id, service_data }) = event
                    else {
                        continue;
                    };
                    if !service_data.contains_key(&service) {
                        continue;
                    }
                    let name = name_of(central, &id).await;
                    // An unnamed box cannot match a name. It also cannot be
                    // ruled out — the lookup may simply not have answered yet
                    // — but taking it as a match would connect to a box the
                    // caller did not ask for, which is the one mistake here
                    // that costs hardware rather than time.
                    match &want {
                        Some(want) if !name.to_lowercase().contains(want.as_str()) => continue,
                        _ => {}
                    }
                    return Ok((id, name));
                }
            }
        }
        Err(Error::NoTentacle {
            filter: self.name.clone(),
        })
    }
}

/// One calibration in progress: the round trips are taken, the advertisements
/// are not yet.
///
/// The box has been disconnected by the time a caller sees one of these, so
/// every advertisement fed in from here is free-running by construction —
/// which is why [`advert`](Pass::advert) takes no [`Link`] and there is no way
/// to record one wrongly. Getting that backwards is what produced the wrong
/// first answer to this question; see [`crate::jam`].
#[derive(Debug)]
pub struct Pass {
    device: PeripheralId,
    name: String,
    round_trips: usize,
    connected_for: Duration,
    adverts: Adverts,
}

/// The advertisement half of a [`Pass`]: the calibrator holding the round
/// trips, and the window that decides what else reaches it.
///
/// Split out from [`Pass`] so the window rule can be exercised without a
/// `PeripheralId`, which no test can conjure — the rule is the whole of this
/// module's answer to drift it cannot de-trend, so it wants a test that fails
/// when it goes.
#[derive(Debug)]
struct Adverts {
    calibrator: jam::Calibrator,
    opened: Instant,
    window: Duration,
    taken: usize,
}

impl Adverts {
    fn new(calibrator: jam::Calibrator, opened: Instant, window: Duration) -> Adverts {
        Adverts {
            calibrator,
            opened,
            window,
            taken: 0,
        }
    }

    fn take(&mut self, timecode: &Timecode, at: Instant) -> bool {
        if !self.open(at) {
            return false;
        }
        self.calibrator.advert(timecode, at, Link::Down);
        self.taken += 1;
        true
    }

    fn open(&self, now: Instant) -> bool {
        now.duration_since(self.opened) < self.window
    }
}

impl Pass {
    /// Which box this was measured on.
    ///
    /// Worth keeping hold of. The constant belongs to this box and this host,
    /// and a display showing three boxes has two it was not measured on — see
    /// [`Device::jam`](crate::ble::Device::jam).
    pub fn device(&self) -> &PeripheralId {
        &self.device
    }

    /// What that box calls itself, or `<unnamed>` if it answered no properties
    /// lookup.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Round trips taken. About a hundred is what one connection gives.
    pub fn round_trips(&self) -> usize {
        self.round_trips
    }

    /// How long the link lasted. About 6.6 s, and the box's decision rather
    /// than this crate's.
    pub fn connected_for(&self) -> Duration {
        self.connected_for
    }

    /// Free-running advertisements taken in so far.
    pub fn adverts(&self) -> usize {
        self.adverts.taken
    }

    /// Feeds one of this box's advertisements in, and says whether it counted.
    ///
    /// `at` wants to be when the advertisement came off the stream, stamped
    /// before any lookup or format — the same rule
    /// [`FreeRun::anchor`](crate::freerun::FreeRun::anchor) states, for the
    /// same reason.
    ///
    /// `false` means the window has closed and the reading was dropped, which
    /// is not a failure: see [`ADVERT_WINDOW`] for why a late advertisement
    /// carries drift this cannot separate from delivery. Feed only the box
    /// [`device`](Pass::device) names; another Tentacle on the same timeline is
    /// a second unknown offset, not a second reading of this one.
    pub fn advert(&mut self, timecode: &Timecode, at: Instant) -> bool {
        self.adverts.take(timecode, at)
    }

    /// Whether advertisements are still being pooled as of `now`.
    pub fn open(&self, now: Instant) -> bool {
        self.adverts.open(now)
    }

    /// When the window shuts, after which [`advert`](Pass::advert) drops
    /// everything.
    pub fn closes_at(&self) -> Instant {
        self.adverts.opened + self.adverts.window
    }

    /// Reduces what has been collected, or says why it cannot be.
    ///
    /// Cheap enough to call on every advertisement, which is the way to use it:
    /// take the first answer that is both `Ok` and
    /// [`settled`](crate::jam::Calibration::settled) rather than waiting the
    /// window out. Until then expect
    /// [`Flaw::NoFreeRunning`](crate::jam::Flaw::NoFreeRunning), which is what
    /// a pass with the round trips in and no advertisements yet honestly is.
    pub fn finish(&self) -> std::result::Result<jam::Calibration, jam::Flaw> {
        self.adverts.calibrator.finish()
    }
}

/// Connects, bounded, and turns a failure into something printable.
async fn connect(peripheral: &Peripheral) -> std::result::Result<(), String> {
    match tokio::time::timeout(CONNECT_GIVE_UP, peripheral.connect()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "no answer in {:.0}s",
            CONNECT_GIVE_UP.as_secs_f64()
        )),
    }
}

/// Reads the timecode characteristic on the dither schedule until the box hangs
/// up, and hands back how many round trips that was.
///
/// Nothing is subscribed to, here or anywhere in this module. See the module
/// docs: a read taken while subscribed is resolved by the notification stream
/// and its timing is fiction.
async fn converse(peripheral: &Peripheral, calibrator: &mut jam::Calibrator) -> Result<usize> {
    peripheral.discover_services().await?;
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|ch| ch.uuid == TIMECODE_CHAR)
        .ok_or(Error::NoTimecodeCharacteristic)?;

    let started = Instant::now();
    let mut reads = 0u64;
    let mut round_trips = 0usize;
    while started.elapsed() < SESSION_CAP {
        // The experiment, and all of it: a stamp either side of a round trip,
        // with nothing between `t0` and the request but the read itself and
        // nothing between the response and `t1`.
        let t0 = Instant::now();
        let read = tokio::time::timeout(READ_GIVE_UP, peripheral.read(&characteristic)).await;
        let t1 = Instant::now();
        // A link that goes away is the normal end of a session on this box, not
        // an error: about 6.6 s in it hangs up whatever the client is doing.
        let Ok(Ok(bytes)) = read else { break };
        match vendor_timecode(&bytes) {
            Some(tc) => {
                calibrator.round_trip(&tc, t0, t1);
                round_trips += 1;
            }
            None => calibrator.undecoded(),
        }
        reads += 1;
        tokio::time::sleep(dither(reads)).await;
    }
    Ok(round_trips)
}

/// Scans briefly so the adapter remembers the peripheral, and hands back a
/// fresh handle to it.
async fn rediscover(central: &Adapter, id: &PeripheralId) -> Result<Peripheral> {
    central.start_scan(ScanFilter::default()).await?;
    let give_up = Instant::now() + CONNECT_GIVE_UP;
    let found = loop {
        match central.peripheral(id).await {
            Ok(p) => break Some(p),
            Err(_) if Instant::now() < give_up => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(_) => break None,
        }
    };
    let _ = central.stop_scan().await;
    found.ok_or_else(|| Error::NoTentacle { filter: None })
}

async fn name_of(central: &Adapter, id: &PeripheralId) -> String {
    match central.peripheral(id).await {
        Ok(p) => p
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|props| props.local_name.or(props.advertisement_name))
            .unwrap_or_else(|| "<unnamed>".into()),
        Err(_) => "<unnamed>".into(),
    }
}

/// How long to wait before the `n`th read, so that `t0` sweeps the connection
/// anchor grid instead of landing on one phase of it.
///
/// A read handed to the controller waits for the next connection anchor before
/// it goes anywhere, so most of what a round trip measures is where `t0`
/// happened to fall in the 30 ms between two of them. That is not a nuisance to
/// be averaged away — **the shortest round trip is the entire measurement**, and
/// it only happens when `t0` lands just before an anchor. Poll on a fixed
/// cadence and `t0` can sit at one phase of that grid for a whole session,
/// putting a floor under the round trip that belongs to the polling and not to
/// the link.
///
/// Stepping by a prime number of microseconds across a span slightly wider than
/// the interval walks every phase of it, since a step sharing no factor with
/// the span visits the whole of it before repeating — and the span is
/// deliberately not the measured 30 ms, so the sweep still covers a whole
/// interval if the real one turns out a little different.
fn dither(n: u64) -> Duration {
    /// Coprime with `SPAN`, so the sequence visits every microsecond of it
    /// before it repeats.
    const STEP: u64 = 7_919;
    /// A little over the 30 ms connection interval `PROTOCOL.md` measures.
    const SPAN: u64 = 31_000;
    Duration::from_micros(n.wrapping_mul(STEP) % SPAN)
}

/// A vendor characteristic payload as a timecode.
///
/// `0dab144c` carries the advertisement's `0x22` record with its two header
/// bytes taken off — `PROTOCOL.md` has the byte-by-byte evidence — so putting a
/// header back on and handing it to [`ble::parse`] is the whole of the decode,
/// and reuses the range checks that keep some other vendor's blob from being
/// read as a clock. Byte 1 is skipped rather than read, so what goes there
/// doesn't matter.
fn vendor_timecode(bytes: &[u8]) -> Option<Timecode> {
    let mut framed = Vec::with_capacity(ble::HEADER + bytes.len());
    framed.extend_from_slice(&[KIND_TIMECODE, 0]);
    framed.extend_from_slice(bytes);
    match ble::parse(&framed)? {
        Advert::Timecode(tc) => Some(tc),
        Advert::Date(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timecode::Rate;

    /// The measured box, as a model: a 30 ms connection interval, floors of
    /// 1.7 ms each way on a round trip and 0.35 ms on an advertisement, and the
    /// two clocks 8.6 ppm apart. Those reproduce `PROTOCOL.md`'s capture to a
    /// tenth of a millisecond.
    const INTERVAL: f64 = 0.030;
    const OUT_FLOOR: f64 = 0.0017;
    const RET_FLOOR: f64 = 0.0017;
    const ADVERT_FLOOR: f64 = 0.00035;
    const DRIFT: f64 = 8.6e-6;

    /// The device's clock minus this host's, `host` seconds in. Deliberately
    /// huge, as it is in life — a time of day against an interval — so any
    /// arithmetic that fails to cancel it shows up enormous.
    fn theta(host: f64) -> f64 {
        10.0 * 3600.0 + DRIFT * host
    }

    fn stamp(device: f64) -> Timecode {
        Timecode::at_frame_position(device * 24.0, Rate::whole(24))
    }

    fn dur(secs: f64) -> Duration {
        Duration::from_secs_f64(secs.max(0.0))
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// One connection's worth of round trips, from `offset` seconds into the
    /// calibrator's timeline, with `t0` swept by the same dither the pass uses.
    ///
    /// The request waits out the rest of the interval and the response, already
    /// at the device, waits not at all — which is why the two minima are
    /// achieved by different samples and the bracket comes out far narrower
    /// than the shortest round trip.
    fn connection(cal: &mut jam::Calibrator, base: Instant, offset: f64) {
        for n in 1..=200u64 {
            let t0 = offset + n as f64 * 0.033;
            let phase = dither(n).as_secs_f64().min(INTERVAL);
            let d_out = OUT_FLOOR + (INTERVAL - phase);
            let stamped = t0 + d_out;
            cal.round_trip(
                &stamp(stamped + theta(stamped)),
                base + dur(t0),
                base + dur(t0 + d_out + RET_FLOOR + phase),
            );
        }
    }

    /// One free-running advertisement, `delay` behind the device's own stamp.
    fn advert(cal: &mut jam::Calibrator, base: Instant, at: f64, delay: f64) {
        let stamped = at - delay;
        cal.advert(&stamp(stamped + theta(stamped)), base + dur(at), Link::Down);
    }

    #[test]
    fn the_dither_sweeps_the_connection_interval_rather_than_landing_on_it() {
        // The claim in `dither`, and the one the whole measurement rests on:
        // over a session's worth of reads the waits have to land all over the
        // 30 ms anchor grid. A step sharing a factor with the span would
        // revisit a handful of phases forever, and the shortest round trip —
        // which is the answer — would never be sampled at all.
        const INTERVAL: u64 = 30_000;
        // About what one 6.6 s connection gets through.
        let mut phases: Vec<u64> = (1..=200)
            .map(|n| dither(n).as_micros() as u64 % INTERVAL)
            .collect();
        phases.sort_unstable();
        let widest = phases
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .expect("two hundred waits");
        // 193 µs as it stands, against the 1,000 µs a step of 1,000 µs would
        // leave and the 6,200 µs a step of 6,200 would.
        assert!(
            widest < INTERVAL / 50,
            "widest unsampled phase gap {widest} µs"
        );
    }

    #[test]
    fn one_block_makes_the_answer_independent_of_when_the_connection_happened() {
        // Why `ONE_BLOCK` is not tidiness. Blocks are cut on a fixed grid from
        // the calibrator's origin, so where a 6.6 s connection falls against
        // that grid is decided by how long the hunt for a box took — nothing to
        // do with the measurement. Land astride a boundary and `finish` has two
        // midpoints instead of one; two points always define a line, and the
        // line it fits here runs through six seconds of jitter and then
        // de-trends every advertisement pooled afterwards.
        //
        // So the property to hold is not "the split is wrong" but "the answer
        // must not depend on the accident". These offsets put the boundary in
        // the middle of the reads, near the front, and near the back.
        let offsets = [23.5, 24.0, 25.0, 27.0, 29.5, 29.9, 29.95];
        let feed = |block: Duration, offset: f64| {
            let base = Instant::now();
            let mut cal = jam::Calibrator::since(base).with_block(block);
            connection(&mut cal, base, offset);
            // A minute of free-running advertisements afterwards, with jitter
            // reaching zero every eleventh so the floor is genuinely achieved
            // and not merely approached.
            for k in 0..90u64 {
                let jitter = ((k * 37) % 11) as f64 * 0.0005;
                advert(
                    &mut cal,
                    base,
                    offset + 7.0 + k as f64 * 0.7,
                    ADVERT_FLOOR + jitter,
                );
            }
            cal.finish()
        };

        // One block: the same answer to the microsecond, wherever it landed.
        let whole: Vec<_> = offsets
            .iter()
            .map(|&at| feed(ONE_BLOCK, at).expect("a clean capture"))
            .collect();
        for (at, done) in offsets.iter().zip(&whole) {
            assert_eq!(done.blocks().len(), 1, "ONE_BLOCK split at {at}");
            // To the microsecond, which is a thousandth of the spread the 30 s
            // grid shows below. Not to the nanosecond: these are f64 seconds
            // built from a device time of day, so the last digit is noise.
            assert!(
                done.offset().abs_diff(whole[0].offset()) < Duration::from_micros(1),
                "offset moved to {:.3} ms because the connection happened at {at} s",
                ms(done.offset())
            );
        }

        // On the 30 s grid it is a lottery. Two of these offsets are refused
        // outright — a thin block's minima are biased enough that the fitted
        // slope pushes the pooled bounds past each other — and the ones that do
        // reduce disagree with each other by a third of a millisecond, which is
        // a third of the whole correction being applied.
        let split: Vec<_> = offsets.iter().map(|&at| feed(jam::BLOCK, at)).collect();
        assert!(
            split.iter().any(|r| r.is_err()),
            "no offset was refused on the 30 s grid, so this test is no longer \
             exercising the thin-block hazard"
        );
        let survived: Vec<f64> = split.iter().flatten().map(|d| ms(d.offset())).collect();
        let spread = survived.iter().cloned().fold(f64::MIN, f64::max)
            - survived.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            spread > 0.25,
            "the surviving 30 s-grid captures agreed to {spread:.3} ms, so a \
             split block no longer moves the answer and ONE_BLOCK buys nothing"
        );
    }

    #[test]
    fn a_vendor_payload_decodes_as_the_headerless_record_it_is() {
        // PROTOCOL.md's worked example off `0dab144c`: the advertisement's 0x22
        // record with its two header bytes taken away. Framing it back up
        // wrongly — one byte instead of two, or the wrong record type — moves
        // every field, and every round trip would carry a stamp from the wrong
        // place in the packet.
        let tc = vendor_timecode(&[0x19, 0x0c, 0x24, 0x33, 0x05, 0x67, 0x51])
            .expect("seven bytes of headerless timecode");
        assert_eq!(tc.rate.fps, 25);
        assert_eq!((tc.hours, tc.minutes, tc.seconds, tc.frames), (12, 36, 51, 5));
        assert_eq!(tc.subframe.as_micros(), 0x6751);

        // The other two vendor characteristics do have bytes in them, and read
        // as a clock they would put nonsense on the timeline. `0dab2496`:
        assert!(vendor_timecode(&[0u8; 24]).is_none());
        // `0dab1280`, Ricki's state record from the sync in PROTOCOL.md:
        assert!(
            vendor_timecode(&[
                0x0d, 0x6c, 0x00, 0x0c, 0x01, 0x00, 0x53, 0x19, 0x00, 0x00, 0x04, 0x09, 0x1a,
                0x0b, 0x1c, 0x28,
            ])
            .is_none()
        );
    }

    #[test]
    fn an_advertisement_past_the_window_never_reaches_the_calibrator() {
        // The window is the whole of this module's answer to drift it cannot
        // de-trend, so a late reading has to be refused rather than merely
        // discouraged. Refused where it counts, too: not just uncounted, but
        // absent from the arithmetic.
        let base = Instant::now();
        let mut cal = jam::Calibrator::since(base).with_block(ONE_BLOCK);
        connection(&mut cal, base, 0.0);

        // The window opens at the disconnect, 7 s in, and runs 10 s.
        let opened = base + dur(7.0);
        let mut adverts = Adverts::new(cal, opened, Duration::from_secs(10));

        assert!(adverts.take(&stamp(theta(8.0) + 8.0), base + dur(8.0)));
        assert!(adverts.open(base + dur(16.999)));
        // Exactly at the boundary is shut: the window is how long it is, not
        // one reading longer.
        assert!(!adverts.open(base + dur(17.0)));
        assert!(!adverts.take(&stamp(theta(17.0) + 17.0), base + dur(17.0)));
        assert!(!adverts.take(&stamp(theta(90.0) + 90.0), base + dur(90.0)));
        assert_eq!(adverts.taken, 1, "only the one inside the window counted");

        // And the same again with nothing inside the window at all. If a late
        // advertisement reached the calibrator, this would reduce cleanly
        // instead of refusing — which is the failure worth catching, since a
        // capture that reduces is one somebody quotes.
        let mut cal = jam::Calibrator::since(base).with_block(ONE_BLOCK);
        connection(&mut cal, base, 0.0);
        let mut late = Adverts::new(cal, opened, Duration::from_secs(10));
        for k in 0..40u64 {
            let jitter = ((k * 37) % 11) as f64 * 0.0005;
            let at = 30.0 + k as f64 * 0.7;
            assert!(!late.take(
                &stamp(theta(at - ADVERT_FLOOR - jitter) + at - ADVERT_FLOOR - jitter),
                base + dur(at),
            ));
        }
        assert!(
            matches!(late.calibrator.finish(), Err(jam::Flaw::NoFreeRunning)),
            "a late advertisement got into the calibrator"
        );
    }

    #[test]
    fn a_wider_window_loses_offset_to_drift_and_eventually_refuses_the_capture() {
        // The claim the module docs make, and the whole reason the window is
        // bounded. `a` holds theta as it was during the connection and `b`
        // holds minus-theta whenever the advertisement landed, so a later
        // advertisement has a smaller `b` and wins the floor for a reason that
        // is not delivery. Nothing here can de-trend it — one connection is one
        // block and one block fits no slope — so the staleness, and with it the
        // offset, comes out too *small*.
        //
        // No jitter on these, so the floor is achieved by the last
        // advertisement in the window and the arithmetic is exact.
        let last_advert = |window: f64| {
            let mut at = 7.0;
            while at + 0.7 - 7.0 < window {
                at += 0.7;
            }
            at
        };
        let measure = |window: f64| {
            let base = Instant::now();
            let mut cal = jam::Calibrator::since(base).with_block(ONE_BLOCK);
            connection(&mut cal, base, 0.0);
            let mut at = 7.0;
            while at - 7.0 < window {
                advert(&mut cal, base, at, ADVERT_FLOOR);
                at += 0.7;
            }
            cal.finish()
        };

        let short = measure(20.0).expect("a clean capture");
        let long = measure(200.0).expect("a clean capture");

        // Exactly the drift over the extra span, halved because the offset is a
        // bracket midpoint and only the top end moved. Predicted from the model
        // rather than pinned to a number, so the test says *why* it is that
        // much.
        let lost = ms(short.offset()) - ms(long.offset());
        let predicted = DRIFT * (last_advert(200.0) - last_advert(20.0)) * 1e3 / 2.0;
        assert!(
            lost > 0.0,
            "a wider window must lose offset, not gain it: {lost:+.3} ms"
        );
        assert!(
            (lost - predicted).abs() < 0.02,
            "the wider window lost {lost:.3} ms; drift over the extra \
             {:.0} s predicts {predicted:.3}",
            last_advert(200.0) - last_advert(20.0)
        );

        // And past about four minutes it stops under-reporting and starts
        // refusing: the drift exceeds the 2.05 ms the two floors leave, the
        // pooled bounds on theta cross, and `finish` says so rather than
        // handing back a number near zero. Worth knowing — the window is the
        // first line of defence and this is the second.
        assert!(
            matches!(measure(400.0), Err(jam::Flaw::StreamsContradict { .. })),
            "a 400 s window should have contradicted, not reduced"
        );
    }
}
