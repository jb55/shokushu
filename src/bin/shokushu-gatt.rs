//! Listen to the vendor GATT service, `0xfdac`, and report what it pushes.
//!
//! `shokushu-probe` establishes what the GATT tree *is*. This asks what the
//! vendor characteristics in it *say*. Three of the four are `READ | NOTIFY`,
//! so the question can be answered without writing anything: read each one,
//! subscribe to all three, and log every notification with a host timestamp.
//! With `--scan` the device's advertisements go on the same timeline, at a
//! cost described under `converse`.
//!
//! What it found, in short. `0dab144c` carries the timecode with the
//! advertisement's two header bytes taken off — five data bytes and the same
//! big-endian microsecond trailer — pushed once per 30 ms connection event, so
//! every frame of a 25 fps device arrives against the advertisement's 1.4–1.8
//! readings a second. `0dab1280` is a length-prefixed state record holding the
//! frame rate and the date and time of the last sync; it never notifies and
//! changes only when the box is synced. `0dab2496` is twenty-four zero bytes
//! and has never done anything. `PROTOCOL.md` has the evidence.
//!
//! The box drops any connection after about 6.6 s, whatever the client does —
//! subscribed and taking 33 notifications a second, or subscribed to nothing
//! and reading once a second, it makes no difference. `--reconnect` exists
//! because of that and is the only way to watch this service for a minute.
//!
//! Nothing here writes. The fourth characteristic, `0dab17e4`, is write-only
//! and is how the Tentacle app sets a device's clock and name; a guessed
//! payload there is how a box ends up needing a factory reset. It is listed
//! and left alone.
//!
//! Like `shokushu-probe` and unlike `shokushu-ble`, this opens a connection,
//! which can disturb the device's advertising. It is deliberately a separate
//! binary for that reason — the scanner stays passive.
//!
//! The closing summary is the point of the tool, not the log. For each
//! characteristic it prints how many notifications arrived, how many were
//! distinct, what lengths were seen, and how many distinct values each byte
//! position took — which is the same "which bytes move, and how fast" reading
//! that decoded the advertisement.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, CharPropFlags, Central, CentralEvent, Manager as _, Peripheral as _,
    ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use clap::Parser;
use futures::stream::StreamExt;
use shokushu::ble::{self, Advert};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(version, about = "Listen to a Tentacle Sync E's vendor GATT service")]
struct Opt {
    /// Only connect to devices whose advertised name contains this.
    #[arg(long)]
    name: Option<String>,

    /// How long to listen for notifications. 0 listens until killed.
    #[arg(long, default_value_t = 120)]
    seconds: u64,

    /// How long to scan for Tentacles before connecting.
    #[arg(long, default_value_t = 12)]
    scan_seconds: u64,

    /// Keep scanning while connected, to log advertisements beside
    /// notifications. Costs most of the connection: see the note in `converse`.
    #[arg(long)]
    scan: bool,

    /// Re-read the readable characteristics this often, in milliseconds, and
    /// log any that changed. 0 reads them only once, when connecting — which
    /// also leaves `--reconnect` with nothing to notice a dead link by.
    #[arg(long, default_value_t = 5000)]
    poll_ms: u64,

    /// Don't subscribe to anything, only read. Isolates whether the
    /// notification traffic is what ends the connection.
    #[arg(long)]
    no_subscribe: bool,

    /// Reconnect and resubscribe each time the box drops the link, until the
    /// run's time is up. The only way to watch this service for longer than
    /// the seven seconds a connection survives.
    #[arg(long)]
    reconnect: bool,
}

/// The vendor service, the same 16-bit UUID the timecode is advertised under.
const VENDOR_SERVICE_16: u16 = ble::SERVICE_UUID_16;

/// Client Characteristic Configuration, the descriptor a subscribe writes.
const CCCD: u16 = 0x2902;

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    let manager = Manager::new().await?;
    let central = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no bluetooth adapter"))?;

    let service = uuid_from_u16(ble::SERVICE_UUID_16);
    let mut events = central.events().await?;
    central.start_scan(ScanFilter::default()).await?;

    // Find them by service data, not by name — the name is whatever the owner
    // typed into the app.
    eprintln!("scanning for {service} …");
    let mut found: BTreeMap<PeripheralId, String> = BTreeMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(opt.scan_seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            event = events.next() => {
                let Some(CentralEvent::ServiceDataAdvertisement { id, service_data }) = event else {
                    continue;
                };
                if !service_data.contains_key(&service) || found.contains_key(&id) {
                    continue;
                }
                let name = name_of(&central, &id).await;
                if let Some(want) = &opt.name
                    && !name.contains(want.as_str())
                {
                    continue;
                }
                eprintln!("  found {name}");
                found.insert(id, name);
            }
        }
    }

    let Some((id, name)) = found.into_iter().next() else {
        central.stop_scan().await?;
        return Err(anyhow!("no Tentacle advertising {service}"));
    };
    // Stop scanning before connecting and start again once the connection is
    // established. `shokushu-probe` does the same and connects reliably; this
    // tool at first held the scan up throughout and CoreBluetooth dropped the
    // link during service discovery every time, before a single read. Scanning
    // and connecting at once is evidently more than the adapter wants to do.
    central.stop_scan().await?;
    let result = listen(&central, &id, &name, &opt, &mut events).await;
    let _ = central.stop_scan().await;
    result
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

/// Connect, listen until the box hangs up, and — with `--reconnect` — do it
/// again until the run's time is up.
///
/// One connection is worth about seven seconds (see `Session`), so anything
/// that wants to watch this service for a minute has to keep re-establishing
/// it. Digests accumulate across sessions; each session's life is recorded so
/// the run can report the spread rather than a single number.
async fn listen(
    central: &Adapter,
    id: &PeripheralId,
    name: &str,
    opt: &Opt,
    events: &mut (impl futures::Stream<Item = CentralEvent> + Unpin),
) -> Result<()> {
    let overall = Instant::now();
    let mut run = Run::default();
    let mut peripheral = central.peripheral(id).await?;
    loop {
        eprintln!("connecting to {name} …");
        if let Err(e) = peripheral.connect().await {
            println!(
                "[{:7.3}s] connect failed: {e}",
                overall.elapsed().as_secs_f64()
            );
            if !opt.reconnect || expired(opt, &overall) {
                break;
            }
            // CoreBluetooth forgets a peripheral that isn't being scanned for,
            // so after a drop the cached handle can go stale and reconnecting
            // through it fails with "Device not found". A short scan puts it
            // back in the adapter's cache.
            peripheral = rediscover(central, id).await?;
            continue;
        }
        let session = converse(central, &peripheral, name, opt, events, &overall, &mut run).await;
        let _ = timeout(peripheral.disconnect()).await;
        match session {
            Ok(life) => run.lives.push(life),
            Err(e) => println!(
                "[{:7.3}s] session failed: {e}",
                overall.elapsed().as_secs_f64()
            ),
        }
        if !opt.reconnect || expired(opt, &overall) {
            break;
        }
    }
    eprintln!("disconnected");
    println!("\n{}", run.summarise(overall.elapsed()));
    Ok(())
}

/// Give up on a GATT teardown that isn't going to answer. Every one of these
/// is best-effort: the alternative to a bounded wait is hanging on a link the
/// device has already dropped.
async fn timeout<T>(op: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(3), op).await.ok()
}

/// Scan briefly so the adapter remembers the peripheral, and hand back a
/// fresh handle to it.
async fn rediscover(central: &Adapter, id: &PeripheralId) -> Result<Peripheral> {
    central.start_scan(ScanFilter::default()).await?;
    let give_up = Instant::now() + Duration::from_secs(15);
    let found = loop {
        match central.peripheral(id).await {
            Ok(p) => break Some(p),
            Err(_) if Instant::now() < give_up => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(_) => break None,
        }
    };
    central.stop_scan().await?;
    // A box that has left the room is a legitimate end to the run, not
    // something to spin on.
    found.ok_or_else(|| anyhow!("{id} stopped advertising"))
}

/// Whether the run's overall deadline has passed. `--seconds 0` never expires.
fn expired(opt: &Opt, overall: &Instant) -> bool {
    opt.seconds > 0 && overall.elapsed() >= Duration::from_secs(opt.seconds)
}

async fn converse(
    central: &Adapter,
    peripheral: &Peripheral,
    name: &str,
    opt: &Opt,
    events: &mut (impl futures::Stream<Item = CentralEvent> + Unpin),
    overall: &Instant,
    run: &mut Run,
) -> Result<Duration> {
    let id = peripheral.id();
    peripheral.discover_services().await?;

    let vendor = uuid_from_u16(VENDOR_SERVICE_16);
    let mut readable = Vec::new();
    let mut initial: BTreeMap<Uuid, Vec<u8>> = BTreeMap::new();
    let first = run.sessions == 0;
    run.sessions += 1;
    if first {
        println!("\n=== {name}: vendor service {vendor}");
    }
    for ch in peripheral.characteristics() {
        if ch.service_uuid != vendor {
            continue;
        }
        // The tree is the same every session; printing it each reconnect
        // would bury the log it exists to introduce.
        if first {
            println!("  {}  {:?}", ch.uuid, ch.properties);
        }
        // Only worth reading a descriptor that might name the characteristic.
        // A Client Characteristic Configuration (0x2902) is just our own
        // subscribe bits reflected back, and reading one is not free — it is a
        // round trip on a link this device is quick to drop.
        for d in &ch.descriptors {
            if short(&d.uuid) == Some(CCCD) {
                if first {
                    println!("      descriptor {}  (client config, not read)", d.uuid);
                }
                continue;
            }
            match peripheral.read_descriptor(d).await {
                Ok(bytes) => println!(
                    "      descriptor {}  = {}  {:?}",
                    d.uuid,
                    hex(&bytes),
                    String::from_utf8_lossy(&bytes)
                ),
                Err(e) => println!("      descriptor {}  read failed: {e}", d.uuid),
            }
        }
        if ch.properties.contains(CharPropFlags::READ) {
            match peripheral.read(&ch).await {
                Ok(bytes) => {
                    if first {
                        println!("      read = {}{}", hex(&bytes), gloss(&bytes));
                    }
                    initial.insert(ch.uuid, bytes);
                }
                Err(e) => println!("      read failed: {e}"),
            }
            readable.push(ch.clone());
        }
        if ch.properties.contains(CharPropFlags::NOTIFY) && !opt.no_subscribe {
            match peripheral.subscribe(&ch).await {
                Ok(()) => {
                    if first {
                        println!("      subscribed");
                    }
                }
                Err(e) => println!("      subscribe failed: {e}"),
            }
        }
    }

    let mut notifications = peripheral.notifications().await?;

    // Scanning while connected is optional and off by default because it
    // costs the connection. Held up through the connect it dropped the link
    // during service discovery every time; restarted after subscribing it let
    // the link live about seven seconds. Without it the connection stays up.
    // So the advertisement can be a companion stream or the notifications can
    // be a long one, but not both — and the notification carries the timecode
    // anyway, so the long capture is usually the better trade.
    if opt.scan {
        central.start_scan(ScanFilter::default()).await?;
    }
    let start = Instant::now();
    let mut lived = Duration::ZERO;
    for ch in &readable {
        if let Some(bytes) = initial.get(&ch.uuid) {
            run.polls.entry(ch.uuid).or_default().add(bytes);
            run.note(ch.uuid, bytes, overall, "READ  ");
        }
    }

    if first {
        println!("\nlistening for {}s …", opt.seconds);
    }
    // The deadline is the whole run's, not this session's: a session ends when
    // the box hangs up, which is what the timing here is trying to measure.
    let remaining = match opt.seconds {
        0 => None,
        n => Some(Duration::from_secs(n).saturating_sub(overall.elapsed())),
    };
    let deadline = async move {
        match remaining {
            None => std::future::pending::<()>().await,
            Some(left) => tokio::time::sleep(left).await,
        }
    };
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(opt.poll_ms.max(1)));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await; // the first tick is immediate, and we just read them
    let mut dead = false;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            note = notifications.next() => {
                let Some(note) = note else {
                    println!("[{:7.3}s] notification stream ended", overall.elapsed().as_secs_f64());
                    break;
                };
                lived = start.elapsed();
                println!(
                    "[{:7.3}s] NOTIFY {}  {}{}",
                    overall.elapsed().as_secs_f64(),
                    tail(&note.uuid),
                    hex(&note.value),
                    gloss(&note.value),
                );
                run.digests.entry(note.uuid).or_default().add(&note.value);
            }
            _ = poll.tick(), if opt.poll_ms > 0 => {
                for ch in &readable {
                    match peripheral.read(ch).await {
                        Ok(bytes) => {
                            run.polls.entry(ch.uuid).or_default().add(&bytes);
                            lived = start.elapsed();
                            run.note(ch.uuid, &bytes, overall, "READ  ");
                        }
                        Err(e) => {
                            // The link going away is itself a result, and how
                            // long it lasted is the measurement. Give up on
                            // this session rather than logging the same
                            // failure for every characteristic every tick.
                            println!(
                                "[{:7.3}s] READ   {}  failed: {e}",
                                overall.elapsed().as_secs_f64(),
                                tail(&ch.uuid),
                            );
                            dead = true;
                            break;
                        }
                    }
                }
                if dead {
                    break;
                }
            }
            event = events.next(), if opt.scan => {
                let Some(event) = event else { continue };
                let Some(from) = event_peripheral(&event) else { continue };
                let Some(line) = advert_line(&event) else { continue };
                // Two boxes are usually in range and both advertise the same
                // shape, so an unlabelled log invites reading one box's bytes
                // as the other's. Only the connected one is counted.
                let mine = *from == id;
                if mine {
                    run.adverts += 1;
                }
                println!(
                    "[{:7.3}s] {} {line}",
                    overall.elapsed().as_secs_f64(),
                    if mine { name } else { "(other)" },
                );
            }
        }
    }

    // Tidying up over a link that has already gone away hangs rather than
    // failing — an unsubscribe waits for a response that is never coming — so
    // when the box has hung up, leave it hung up. It cleared its own
    // subscriptions when the connection went.
    if !dead {
        for ch in peripheral.characteristics() {
            if ch.service_uuid == vendor && ch.properties.contains(CharPropFlags::NOTIFY) {
                let _ = timeout(peripheral.unsubscribe(&ch)).await;
            }
        }
    }
    Ok(lived)
}

/// What arrived on a characteristic, in the terms that decoded the
/// advertisement: how much, how much of it was new, and which bytes moved.
#[derive(Default)]
struct Digest {
    count: usize,
    unique: std::collections::BTreeSet<Vec<u8>>,
    /// Distinct values seen at each byte position, positionally.
    per_byte: Vec<std::collections::BTreeSet<u8>>,
}

impl Digest {
    fn add(&mut self, value: &[u8]) {
        self.count += 1;
        self.unique.insert(value.to_vec());
        if self.per_byte.len() < value.len() {
            self.per_byte.resize(value.len(), Default::default());
        }
        for (slot, byte) in self.per_byte.iter_mut().zip(value) {
            slot.insert(*byte);
        }
    }

    /// One digit per byte position: how many distinct values that position
    /// took, `.` for one and `+` for more than nine. A run of `.` is a header
    /// or a constant; a run of digits is a field.
    fn volatility(&self) -> String {
        self.per_byte
            .iter()
            .map(|seen| match seen.len() {
                1 => '.',
                n if n <= 9 => char::from_digit(n as u32, 10).unwrap_or('+'),
                _ => '+',
            })
            .collect()
    }

    fn lengths(&self) -> BTreeMap<usize, usize> {
        let mut lengths: BTreeMap<usize, usize> = BTreeMap::new();
        for payload in &self.unique {
            *lengths.entry(payload.len()).or_default() += 1;
        }
        lengths
    }
}

/// Everything one run of the tool saw, across however many connections it
/// took. Sessions are short — see `Run::summarise` — so a minute of watching
/// is a dozen of them, and the counts only mean anything pooled.
#[derive(Default)]
struct Run {
    sessions: usize,
    /// How long each connection survived, in the order they happened.
    lives: Vec<Duration>,
    /// What arrived unprompted, by characteristic.
    digests: BTreeMap<Uuid, Digest>,
    /// What reads returned, by characteristic.
    polls: BTreeMap<Uuid, Digest>,
    /// The last value logged for a characteristic, so only changes get a line.
    last: BTreeMap<Uuid, Vec<u8>>,
    /// Advertisements seen from the target while connected, if scanning.
    adverts: usize,
}

impl Run {
    /// Log a read, but only when it differs from the last one logged. A
    /// characteristic read every few seconds for a minute would otherwise fill
    /// the log with the news that it hasn't changed, burying the ones that
    /// have. The summary reports the unchanged ones.
    fn note(&mut self, uuid: Uuid, bytes: &[u8], overall: &Instant, what: &str) {
        if self.last.get(&uuid).is_some_and(|prev| prev == bytes) {
            return;
        }
        println!(
            "[{:7.3}s] {what} {}  {}{}",
            overall.elapsed().as_secs_f64(),
            tail(&uuid),
            hex(bytes),
            gloss(bytes),
        );
        self.last.insert(uuid, bytes.to_vec());
    }

    /// The closing report: what each characteristic did, and — as much of the
    /// point — what it didn't. A characteristic that pushed nothing across a
    /// run has to be named as silent, or a reader assumes it was never
    /// subscribed. Same for the link lifetimes: their spread is the evidence
    /// that the short life is the device's doing and not a fluke.
    fn summarise(&self, elapsed: Duration) -> String {
        let mut out = format!(
            "summary over {:.1}s in {} connection(s)",
            elapsed.as_secs_f64(),
            self.sessions,
        );
        if self.adverts > 0 {
            let _ = write!(out, " — {} advertisement events from the target", self.adverts);
        }
        out.push('\n');
        if let Some(&shortest) = self.lives.iter().min() {
            let longest = self.lives.iter().max().copied().unwrap_or(shortest);
            let total: Duration = self.lives.iter().sum();
            let _ = writeln!(
                out,
                "  link life over {} sessions: {:.2}s..{:.2}s, mean {:.2}s",
                self.lives.len(),
                shortest.as_secs_f64(),
                longest.as_secs_f64(),
                total.as_secs_f64() / self.lives.len() as f64,
            );
        }

        let seen: std::collections::BTreeSet<_> =
            self.polls.keys().chain(self.digests.keys()).collect();
        for uuid in seen {
            let _ = writeln!(out, "  {}", tail(uuid));
            match self.digests.get(uuid) {
                Some(digest) => {
                    let _ = writeln!(
                        out,
                        "      notify  {} pushed, {} distinct, {:.2}/s, lengths {:?}",
                        digest.count,
                        digest.unique.len(),
                        digest.count as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
                        digest.lengths(),
                    );
                    let _ = writeln!(out, "              volatility {}", digest.volatility());
                }
                None => {
                    let _ = writeln!(out, "      notify  silent for the whole run");
                }
            }
            if let Some(digest) = self.polls.get(uuid) {
                let _ = writeln!(
                    out,
                    "      read    {} reads, {} distinct, volatility {}",
                    digest.count,
                    digest.unique.len(),
                    digest.volatility(),
                );
            }
        }
        out
    }
}

/// A `0xFDAC` service-data or manufacturer advertisement, as a log line.
fn advert_line(event: &CentralEvent) -> Option<String> {
    match event {
        CentralEvent::ServiceDataAdvertisement { service_data, .. } => {
            let data = service_data.get(&uuid_from_u16(ble::SERVICE_UUID_16))?;
            Some(format!("ADV svc  {}{}", hex(data), gloss(data)))
        }
        CentralEvent::ManufacturerDataAdvertisement {
            manufacturer_data, ..
        } => {
            let data = manufacturer_data.get(&ble::COMPANY_ID)?;
            let status = ble::parse_manufacturer(data)
                .map(|s| {
                    format!(
                        "   -> {}%{}",
                        s.battery_percent,
                        if s.charging { " charging" } else { "" }
                    )
                })
                .unwrap_or_default();
            Some(format!("ADV mfg  {}{status}", hex(data)))
        }
        _ => None,
    }
}

/// Whatever the advertisement parsers can make of a payload, if anything. The
/// prior worth testing is that the vendor characteristics speak the same
/// nine-byte records the advertisement does — so decode every blob as one and
/// let the log show where that holds.
fn gloss(bytes: &[u8]) -> String {
    match ble::parse(bytes) {
        Some(Advert::Timecode(tc)) => format!(
            "   -> {tc}  {}fps +{}µs",
            tc.rate.fps,
            tc.subframe.as_micros()
        ),
        Some(Advert::Date(date)) => format!("   -> {date}"),
        None => String::new(),
    }
}

/// Which device an advertisement event came from, for the events that say.
fn event_peripheral(event: &CentralEvent) -> Option<&PeripheralId> {
    match event {
        CentralEvent::ServiceDataAdvertisement { id, .. }
        | CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::DeviceDiscovered(id)
        | CentralEvent::DeviceUpdated(id) => Some(id),
        _ => None,
    }
}

/// The 16-bit form of a Bluetooth SIG UUID, or `None` if it isn't one.
fn short(uuid: &Uuid) -> Option<u16> {
    let bytes = uuid.as_bytes();
    let candidate = u16::from_be_bytes([bytes[2], bytes[3]]);
    (uuid_from_u16(candidate) == *uuid).then_some(candidate)
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The distinguishing head of a vendor UUID. They share everything but the
/// four bytes at the front, so printing all thirty-six characters of each on
/// every line buries the one part that differs.
fn tail(uuid: &Uuid) -> String {
    uuid.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volatility_marks_a_constant_and_a_field() {
        let mut digest = Digest::default();
        digest.add(&[0x22, 0x7d, 0x19, 0x00]);
        digest.add(&[0x22, 0x7d, 0x19, 0x01]);
        digest.add(&[0x22, 0x7d, 0x19, 0x02]);
        // Three fixed bytes, then a position that took three values.
        assert_eq!(digest.volatility(), "...3");
        assert_eq!(digest.count, 3);
        assert_eq!(digest.unique.len(), 3);
    }

    #[test]
    fn volatility_saturates_past_nine() {
        let mut digest = Digest::default();
        for byte in 0..12u8 {
            digest.add(&[0xff, byte]);
        }
        assert_eq!(digest.volatility(), ".+");
    }

    #[test]
    fn digest_tolerates_a_changing_payload_length() {
        let mut digest = Digest::default();
        digest.add(&[1, 2]);
        digest.add(&[1, 2, 3, 4]);
        // The digest widens to the longest payload rather than panicking, and
        // says so in the lengths — a characteristic whose length varies is a
        // finding, not an error.
        assert_eq!(digest.volatility(), "....");
        assert_eq!(digest.lengths(), BTreeMap::from([(2, 1), (4, 1)]));
    }

    #[test]
    fn repeats_count_once_as_unique() {
        let mut digest = Digest::default();
        digest.add(&[9, 9]);
        digest.add(&[9, 9]);
        assert_eq!((digest.count, digest.unique.len()), (2, 1));
    }

    #[test]
    fn summary_reports_the_spread_of_link_lifetimes() {
        let run = Run {
            sessions: 3,
            lives: vec![
                Duration::from_millis(6780),
                Duration::from_millis(6200),
                Duration::from_millis(7100),
            ],
            ..Default::default()
        };
        let report = run.summarise(Duration::from_secs(30));
        assert!(report.contains("3 connection(s)"), "{report}");
        // The spread is the evidence, so all three numbers have to survive.
        assert!(report.contains("6.20s..7.10s"), "{report}");
        assert!(report.contains("mean 6.69s"), "{report}");
    }

    #[test]
    fn summary_names_a_characteristic_that_pushed_nothing() {
        let mut digest = Digest::default();
        digest.add(&[0, 0]);
        let run = Run {
            sessions: 1,
            polls: BTreeMap::from([(uuid_from_u16(0x1234), digest)]),
            adverts: 40,
            ..Default::default()
        };
        let report = run.summarise(Duration::from_secs(60));
        assert!(report.contains("silent for the whole run"), "{report}");
        assert!(report.contains("40 advertisement events"), "{report}");
    }

    #[test]
    fn summary_of_a_run_that_never_connected_says_nothing_about_link_life() {
        let report = Run::default().summarise(Duration::from_secs(5));
        assert!(!report.contains("link life"), "{report}");
    }

    #[test]
    fn gloss_decodes_a_timecode_record() {
        let line = gloss(&[0x22, 0x7d, 0x19, 0x09, 0x22, 0x33, 0x06, 0xa6, 0x50]);
        assert!(line.contains("09:34:51:06"), "{line}");
        assert!(line.contains("25fps"), "{line}");
    }

    #[test]
    fn gloss_is_empty_for_something_that_isnt_an_advert_record() {
        assert_eq!(gloss(&[0x01, 0x02, 0x03]), "");
    }

    #[test]
    fn short_recognises_a_sig_uuid_and_rejects_a_vendor_one() {
        assert_eq!(short(&uuid_from_u16(CCCD)), Some(CCCD));
        let vendor: Uuid = "0dab1280-2cb9-11e6-b67b-9e71128cae77".parse().unwrap();
        assert_eq!(short(&vendor), None);
    }

    #[test]
    fn tail_keeps_the_part_that_differs() {
        let a: Uuid = "0dab1280-2cb9-11e6-b67b-9e71128cae77".parse().unwrap();
        let b: Uuid = "0dab144c-2cb9-11e6-b67b-9e71128cae77".parse().unwrap();
        assert_ne!(tail(&a), tail(&b));
        assert_eq!(tail(&a), "0dab1280");
    }
}
