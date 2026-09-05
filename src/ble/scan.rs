//! Watching for Tentacles over the air.
//!
//! [`ble`] turns bytes into readings; this turns an adapter into bytes, and
//! keeps a [`Device`] per box so several in range don't have to be untangled by
//! the caller. There are two ways to read it, and both are wanted:
//!
//! - [`Scanner::next`] hands out one [`Event`] per arrival, which is what a
//!   logger or a recorder wants.
//! - [`Scanner::devices`] hands out each device's free-running clock, which is
//!   what anything drawing at its own refresh rate wants. Advertisements land
//!   only once or twice a second, so a display that waits for them jumps a
//!   dozen frames at a time; see [`freerun`](crate::freerun).
//!
//! Both views are live at once and cost nothing extra: every timecode
//! advertisement goes to its device's clock on the way past, whether or not
//! anyone reads the event.
//!
//! ```no_run
//! use shokushu::ble::{Event, Scanner};
//!
//! # async fn run() -> shokushu::Result<()> {
//! let mut scan = Scanner::builder().name("ricki").start().await?;
//! while let Some(event) = scan.next().await {
//!     if let Event::Timecode { timecode, .. } = event {
//!         println!("{timecode}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ```no_run
//! # use std::time::Instant;
//! # use shokushu::ble::Scanner;
//! # async fn run(scan: &mut Scanner) {
//! for device in scan.devices() {
//!     if let Some(reading) = device.reading(Instant::now()) {
//!         println!("{:?} {reading:?}", device.name());
//!     }
//! }
//! # }
//! ```
//!
//! # Arrival times
//!
//! Every advertisement is stamped the moment it comes off the stream, before
//! anything that could await. A clock anchor is only as good as the host time
//! paired with it, and the properties lookup on the way through would put an
//! unbounded delay between the two.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use btleplug::api::{
    bleuuid::uuid_from_u16, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use futures::stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::ble::{self, diagnostics, Advert, Date, Status};
use crate::error::{Error, Result};
use crate::freerun::{Drift, FreeRun, Reading};
use crate::Timecode;

/// How often to look for a device that's gone quiet.
///
/// [`Event::Lost`] is the absence of an advertisement rather than one, so
/// nothing else would raise it — a box switched off produces no events at all.
/// A few times a second is plenty for a five-second holdover and costs nothing.
const HOLDOVER_TICK: Duration = Duration::from_millis(200);

/// Something the scanner took in.
///
/// A single advertisement can produce two of these: whatever it decoded to, and
/// the [`Event::Advertised`] carrying the bytes it arrived as. They're separate
/// because a dump wants the payload whether or not this crate understands it,
/// and everything else wants the reading without the packet.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// First sight of a peripheral, Tentacle or not.
    Discovered { id: PeripheralId },

    /// Timecode arrived, and has already gone to this device's clock — which
    /// keeps it as a candidate rather than anchoring on it directly. See
    /// [`freerun`](crate::freerun).
    Timecode {
        id: PeripheralId,
        timecode: Timecode,
        /// When the advertisement came off the stream.
        at: Instant,
    },

    /// The date the device is set to. It comes round far more rarely than the
    /// timecode does.
    Date { id: PeripheralId, date: Date },

    /// The manufacturer record: remaining charge, and whether it's on a cable.
    /// It rides in the same advertisement as the timecode but reaches a scanner
    /// as its own event.
    Battery { id: PeripheralId, status: Status },

    /// A signal-strength update, as reported by the platform.
    ///
    /// Only raised for a `CentralEvent::RssiUpdate`, which a backend needn't
    /// emit at all: 25 s of watching the raw stream on macOS produced 416
    /// `DeviceUpdated` and not one `RssiUpdate`. [`Device::rssi`] is the
    /// reliable way to the same number, since it comes off the properties
    /// lookup instead.
    Rssi { id: PeripheralId, rssi: i16 },

    /// An advertisement under [`SERVICE_UUID_16`](crate::ble::SERVICE_UUID_16)
    /// that didn't decode.
    ///
    /// Worth surfacing rather than dropping: a Tentacle whose wire format has
    /// moved is otherwise indistinguishable from no Tentacle at all, which is a
    /// hole this crate has fallen down before. See [`diagnostics`].
    Unreadable {
        id: PeripheralId,
        payload: Vec<u8>,
    },

    /// The advertisement as it arrived, decoded or not, ours or not.
    Advertised {
        id: PeripheralId,
        data: Advertisement,
        at: Instant,
    },

    /// This device's clock has gone
    /// [`freerun::HOLDOVER`](crate::freerun::HOLDOVER) without an anchor, so
    /// extrapolation has stopped. Raised once per silence, not per tick.
    Lost {
        id: PeripheralId,
        /// The last timecode actually received.
        last: Timecode,
        since: Duration,
    },
}

impl Event {
    /// Which device this concerns.
    pub fn id(&self) -> &PeripheralId {
        match self {
            Event::Discovered { id }
            | Event::Timecode { id, .. }
            | Event::Date { id, .. }
            | Event::Battery { id, .. }
            | Event::Rssi { id, .. }
            | Event::Unreadable { id, .. }
            | Event::Advertised { id, .. }
            | Event::Lost { id, .. } => id,
        }
    }
}

/// Advertisement data as it arrived, before anything interpreted it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Advertisement {
    /// Manufacturer data, keyed by Bluetooth SIG company identifier. The
    /// Tentacle's is [`COMPANY_ID`](crate::ble::COMPANY_ID).
    Manufacturer(HashMap<u16, Vec<u8>>),
    /// Service data, keyed by UUID. The Tentacle's is
    /// [`SERVICE_UUID_16`](crate::ble::SERVICE_UUID_16), as a 128-bit UUID.
    Service(HashMap<Uuid, Vec<u8>>),
}

/// One Tentacle — or one peripheral that might turn out to be something else.
///
/// A scan sees every advertiser in the room, so most of these never send
/// timecode. [`Device::reading`] answering `None` is what tells them apart.
#[derive(Debug)]
pub struct Device {
    id: PeripheralId,
    name: Option<String>,
    rssi: Option<i16>,
    date: Option<Date>,
    battery: Option<Status>,
    clock: FreeRun,
    first_timecode: Option<Instant>,
    /// Whether this device's current silence has already been reported, so one
    /// dropout raises one [`Event::Lost`] rather than five a second.
    lost_reported: bool,
    adverts: u64,
    fdac: u64,
    unparsed: u64,
    unparsed_sample: Option<Vec<u8>>,
}

impl Device {
    fn new(id: PeripheralId) -> Device {
        Device {
            id,
            name: None,
            rssi: None,
            date: None,
            battery: None,
            clock: FreeRun::default(),
            first_timecode: None,
            lost_reported: false,
            adverts: 0,
            fdac: 0,
            unparsed: 0,
            unparsed_sample: None,
        }
    }

    pub fn id(&self) -> &PeripheralId {
        &self.id
    }

    /// The device's advertised name, once a properties lookup has turned one
    /// up. `None` until then, and for a device that doesn't publish one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Signal strength in dBm, as of the last properties lookup — which is
    /// every advertisement, so it tracks. `None` until the first lookup
    /// answers, and for a platform that reports no signal strength at all.
    pub fn rssi(&self) -> Option<i16> {
        self.rssi
    }

    /// The date the device is set to, from the last date record.
    pub fn date(&self) -> Option<Date> {
        self.date
    }

    /// Charge and charging, from the last manufacturer record.
    pub fn battery(&self) -> Option<Status> {
        self.battery
    }

    /// When this device first sent timecode — a stable key to order a display
    /// by, since anything derived from arrival order moves about.
    pub fn first_timecode(&self) -> Option<Instant> {
        self.first_timecode
    }

    /// What this device's clock says now, interpolated between advertisements.
    ///
    /// `None` for a peripheral that has never sent timecode, which is most of
    /// them. Takes `&mut self` because sampling advances the clock's own model:
    /// it never hands out a position lower than the last one it gave.
    pub fn reading(&mut self, now: Instant) -> Option<Reading> {
        self.clock.sample(now)
    }

    /// The last timecode actually received, whatever the clock is doing with
    /// it.
    pub fn last_received(&self) -> Option<Timecode> {
        self.clock.last_received()
    }

    /// Applies a calibrated path constant to this device's clock.
    ///
    /// [`FreeRun::jam`] has what the constant is, why it does not decay, and
    /// why it is usually not worth applying; [`crate::ble::jam`]
    /// measures one. This is the whole of the wiring — the clock does the rest.
    ///
    /// # Why this is per device and not per scan
    ///
    /// It would be less typing to set one constant on the
    /// [`Builder`] and have every clock take it, and
    /// that is deliberately not offered. A calibration is taken against **one
    /// box**. Part of the constant is this host's Bluetooth stack floor, which
    /// the boxes in a room do share; the rest is the bias in the origin of that
    /// box's microsecond counter, which they may or may not — `PROTOCOL.md`
    /// records the bias as a few milliseconds of unknown origin, measured on
    /// one unit, and unknown origin is not the same as shared.
    ///
    /// So applying a one-box measurement to every box in range is an assumption
    /// and not a deduction. It may well be right, and it is available: call
    /// this on each device. What is not available is making it by accident.
    pub fn jam(&mut self, offset: Duration) {
        self.clock.jam(offset);
    }

    /// The path constant being applied to this device's clock. Zero unless
    /// [`jam`](Device::jam) has set one.
    pub fn offset(&self) -> Duration {
        self.clock.offset()
    }

    /// How far this box's clock has been measured to run from this host's.
    ///
    /// The scanner's clock has to work this out to extrapolate, so it costs
    /// nothing to ask; see [`Drift`] for what it is and isn't, and note the
    /// `None` covers a device that hasn't yet been heard from for the ten
    /// seconds a measurement takes as well as one that isn't a Tentacle at all.
    ///
    /// Every device in one scan is measured against the same host clock, which
    /// is what makes two of them comparable: a drift they share is this
    /// computer's and a drift where they differ is theirs.
    pub fn drift(&self) -> Option<Drift> {
        self.clock.drift()
    }

    /// Advertisements taken in from this device, of any kind.
    pub fn adverts(&self) -> u64 {
        self.adverts
    }

    /// Payloads received under the Tentacle's service UUID.
    pub fn payloads(&self) -> u64 {
        self.fdac
    }

    /// ...of those, how many didn't decode, and the last one that didn't.
    ///
    /// Uninteresting until nothing is being displayed, at which point it's the
    /// only thing there is to go on.
    pub fn unparsed(&self) -> u64 {
        self.unparsed
    }

    pub fn unparsed_sample(&self) -> Option<&[u8]> {
        self.unparsed_sample.as_deref()
    }
}

/// Configures a [`Scanner`] before it starts.
#[derive(Debug, Default, Clone)]
pub struct Builder {
    name: Option<String>,
}

impl Builder {
    /// Only take in devices whose name contains `want`, case-insensitively.
    ///
    /// Devices are still discovered and still counted — see [`diagnostics`],
    /// which needs to be able to say "three in range, none named like that".
    pub fn name(mut self, want: impl Into<String>) -> Builder {
        self.name = Some(want.into());
        self
    }

    /// Takes the first adapter the platform offers and starts scanning.
    pub async fn start(self) -> Result<Scanner> {
        let manager = Manager::new().await?;
        let central = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NoAdapter)?;
        self.start_on(central).await
    }

    /// The same, on an adapter you've already chosen.
    pub async fn start_on(self, central: Adapter) -> Result<Scanner> {
        let events = central.events().await?;
        central.start_scan(ScanFilter::default()).await?;
        Ok(Scanner {
            central,
            events,
            filter: self.name.map(|n| n.to_lowercase()),
            tentacle_service: uuid_from_u16(ble::SERVICE_UUID_16),
            devices: Vec::new(),
            index: HashMap::new(),
            pending: VecDeque::new(),
            holdover: tokio::time::interval(HOLDOVER_TICK),
        })
    }
}

/// A running scan.
///
/// Dropping one stops delivering events but leaves the adapter scanning; call
/// [`Scanner::stop`] to put it back.
pub struct Scanner {
    central: Adapter,
    events: std::pin::Pin<Box<dyn Stream<Item = CentralEvent> + Send>>,
    /// Lower-cased, since the comparison is case-insensitive and doing it once
    /// beats doing it per advertisement.
    filter: Option<String>,
    tentacle_service: Uuid,
    devices: Vec<Device>,
    index: HashMap<PeripheralId, usize>,
    /// Events produced but not yet handed out. One stream item can make two,
    /// and one holdover tick can make several; queueing them is also what keeps
    /// [`Scanner::next`] safe to cancel in a `select!`.
    pending: VecDeque<Event>,
    holdover: tokio::time::Interval,
}

impl Scanner {
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Starts a scan on the first adapter, taking in every device in range.
    pub async fn start() -> Result<Scanner> {
        Builder::default().start().await
    }

    /// What the adapter says about itself — worth printing before a scan that
    /// finds nothing, since `PoweredOff` explains it on its own.
    pub async fn adapter_state(&self) -> Result<btleplug::api::CentralState> {
        Ok(self.central.adapter_state().await?)
    }

    /// The next thing to happen. Never returns `None` while the adapter lives;
    /// `None` means the event stream ended under us.
    ///
    /// Safe to cancel — in a `tokio::select!` arm, say — since everything it
    /// produces is queued on `self` before being handed out.
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            tokio::select! {
                _ = self.holdover.tick() => self.sweep_for_silence(Instant::now()),
                event = self.events.next() => {
                    // Stamped before the properties lookup below, which awaits.
                    let at = Instant::now();
                    self.take_in(event?, at).await;
                }
            }
        }
    }

    /// Every device seen so far, in the order they were first noticed.
    ///
    /// Includes peripherals that have never sent timecode; [`Device::reading`]
    /// is what sorts Tentacles from the rest of the room.
    pub fn devices(&mut self) -> impl Iterator<Item = &mut Device> {
        self.devices.iter_mut()
    }

    /// The same, without the clocks — enough to look a device up by id.
    pub fn device(&self, id: &PeripheralId) -> Option<&Device> {
        self.index.get(id).map(|i| &self.devices[*i])
    }

    /// One device by id, to sample its clock or to
    /// [`jam`](Device::jam) it.
    ///
    /// `None` for a device this scan has not heard from yet, which includes one
    /// it heard from before the scan started — a calibration taken on a
    /// separate scan hands back a `PeripheralId` that only resolves here once
    /// that box has advertised again.
    pub fn device_mut(&mut self, id: &PeripheralId) -> Option<&mut Device> {
        self.index.get(id).copied().map(|i| &mut self.devices[i])
    }

    /// What the name filter was set to, if it was.
    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// What this scan has taken in, across every device.
    pub fn census(&self) -> diagnostics::Census {
        diagnostics::census(&self.devices)
    }

    /// Why nothing is arriving.
    ///
    /// Only worth asking once a scan has been running long enough that a
    /// healthy Tentacle would have been heard from — adverts come about three
    /// times a second per device, but reception is bursty enough that a couple
    /// of seconds of nothing is ordinary. See [`diagnostics`] for what it can
    /// and can't tell apart, and note that it carries no wording: the caller
    /// writes the sentence.
    pub fn diagnosis(&self) -> diagnostics::Diagnosis {
        diagnostics::diagnose(&self.census(), self.filter.is_some())
    }

    /// Stops the scan. Dropping the scanner without this leaves the adapter
    /// scanning, which is the platform's business rather than ours to undo.
    pub async fn stop(&mut self) -> Result<()> {
        self.central.stop_scan().await?;
        Ok(())
    }

    async fn take_in(&mut self, event: CentralEvent, at: Instant) {
        let Some(id) = event_peripheral(&event) else {
            return;
        };
        let fresh = self.ensure(&id);
        self.refresh(&id).await;

        // A filtered-out device is still recorded, just not counted: "three in
        // range, none named like that" is a better answer than an empty screen,
        // and needs the ones that didn't match. Nothing past here is raised for
        // one — including its discovery, which can only be reported once the
        // properties lookup has come back with something to filter on.
        if !self.matches(&id) {
            return;
        }
        let slot = self.index[&id];
        self.devices[slot].adverts += 1;
        if fresh {
            self.pending.push_back(Event::Discovered { id: id.clone() });
        }

        match event {
            CentralEvent::RssiUpdate { rssi, .. } => {
                self.devices[slot].rssi = Some(rssi);
                self.pending.push_back(Event::Rssi { id, rssi });
            }
            CentralEvent::ManufacturerDataAdvertisement {
                manufacturer_data, ..
            } => {
                if let Some(bytes) = manufacturer_data.get(&ble::COMPANY_ID)
                    && let Some(status) = ble::parse_manufacturer(bytes)
                {
                    self.devices[slot].battery = Some(status);
                    self.pending.push_back(Event::Battery {
                        id: id.clone(),
                        status,
                    });
                }
                self.pending.push_back(Event::Advertised {
                    id,
                    data: Advertisement::Manufacturer(manufacturer_data),
                    at,
                });
            }
            CentralEvent::ServiceDataAdvertisement { service_data, .. } => {
                if let Some(payload) = service_data.get(&self.tentacle_service) {
                    self.take_in_payload(slot, &id, payload.clone(), at);
                }
                self.pending.push_back(Event::Advertised {
                    id,
                    data: Advertisement::Service(service_data),
                    at,
                });
            }
            _ => {}
        }
    }

    fn take_in_payload(&mut self, slot: usize, id: &PeripheralId, payload: Vec<u8>, at: Instant) {
        let device = &mut self.devices[slot];
        device.fdac += 1;

        let Some(advert) = ble::parse(&payload) else {
            // Not a reading, but the most useful thing there is to say when no
            // reading ever comes: a Tentacle is right here and we can't read it.
            device.unparsed += 1;
            device.unparsed_sample = Some(payload.clone());
            self.pending.push_back(Event::Unreadable {
                id: id.clone(),
                payload,
            });
            return;
        };

        match advert {
            Advert::Date(date) => {
                device.date = Some(date);
                self.pending.push_back(Event::Date { id: id.clone(), date });
            }
            Advert::Timecode(timecode) => {
                device.first_timecode.get_or_insert(at);
                device.lost_reported = false;
                device.clock.anchor(&timecode, at);
                self.pending.push_back(Event::Timecode {
                    id: id.clone(),
                    timecode,
                    at,
                });
            }
        }
    }

    /// Raises [`Event::Lost`] for any device whose clock has stopped since the
    /// last look.
    fn sweep_for_silence(&mut self, now: Instant) {
        for device in &mut self.devices {
            if device.lost_reported {
                continue;
            }
            let Some(Reading::Lost { last, since }) = device.clock.sample(now) else {
                continue;
            };
            device.lost_reported = true;
            self.pending.push_back(Event::Lost {
                id: device.id.clone(),
                last,
                since,
            });
        }
    }

    /// Returns whether this was the first sight of the device.
    fn ensure(&mut self, id: &PeripheralId) -> bool {
        if self.index.contains_key(id) {
            return false;
        }
        self.index.insert(id.clone(), self.devices.len());
        self.devices.push(Device::new(id.clone()));
        true
    }

    fn matches(&self, id: &PeripheralId) -> bool {
        let Some(want) = &self.filter else {
            return true;
        };
        let device = &self.devices[self.index[id]];
        // An unnamed device can't match a name. It also can't be ruled out yet
        // — the lookup may simply not have answered — but treating it as a
        // match would let every anonymous advertiser through the filter.
        device
            .name
            .as_ref()
            .is_some_and(|name| name.to_lowercase().contains(want))
    }

    /// Asks the platform what it knows about a device: its name, and its
    /// signal strength.
    ///
    /// Neither arrives in an advertisement event, so a lookup is the only way
    /// to either. It runs per advertisement rather than once because
    /// `RssiUpdate` is not guaranteed to arrive at all — on macOS not one was
    /// seen in 25 s of watching the raw stream — and a scan that waits for one
    /// shows no signal strength for the whole of its life. The name is
    /// still only taken once, since it doesn't change and a later lookup
    /// answering `None` shouldn't unname a device that has one.
    ///
    /// Repeating the lookup is not the new cost it looks like: a device that
    /// publishes no name never satisfied the old "once" condition either, so
    /// anonymous advertisers were already being asked on every advertisement.
    ///
    /// Note the arrival `Instant` is stamped by the caller before this is
    /// awaited — see the module docs on arrival times.
    async fn refresh(&mut self, id: &PeripheralId) {
        let slot = self.index[id];
        if let Ok(peripheral) = self.central.peripheral(id).await
            && let Ok(Some(properties)) = peripheral.properties().await
        {
            let device = &mut self.devices[slot];
            if device.name.is_none() {
                device.name = properties.local_name.or(properties.advertisement_name);
            }
            // Keep the last good reading if this lookup didn't carry one.
            device.rssi = properties.rssi.or(device.rssi);
        }
    }
}

fn event_peripheral(event: &CentralEvent) -> Option<PeripheralId> {
    match event {
        CentralEvent::DeviceDiscovered(id)
        | CentralEvent::DeviceUpdated(id)
        | CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::ServiceDataAdvertisement { id, .. }
        | CentralEvent::ServicesAdvertisement { id, .. }
        | CentralEvent::RssiUpdate { id, .. } => Some(id.clone()),
        _ => None,
    }
}
