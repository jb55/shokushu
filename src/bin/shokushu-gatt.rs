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
//! `--phase` is the other reason to open a connection. An advertisement is a
//! one-way broadcast, so nothing that only listens can say how far the
//! device's clock sits from this host's — see the `freerun` module docs. An
//! ATT read is a round trip, which is exactly what that lacks: stamp the host
//! clock either side of one and the timecode in the response brackets the
//! offset. `Phase` has the arithmetic and `analysis/gatt_phase.py` reduces the
//! file it writes.
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
use std::fs::File;
use std::io::{LineWriter, Write as _};
use std::path::{Path, PathBuf};
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
use shokushu::Timecode;
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

    /// Measure the offset between the device's clock and this host's, and
    /// write the samples here as JSON lines. See `Phase` for what it does and
    /// `analysis/gatt_phase.py` for what to do with the file.
    ///
    /// Wants `--no-subscribe`: with a subscription up, a read comes back off
    /// the notification stream instead of from a round trip. See `Phase`.
    #[arg(long, value_name = "FILE")]
    phase: Option<PathBuf>,
}

/// The vendor service, the same 16-bit UUID the timecode is advertised under.
const VENDOR_SERVICE_16: u16 = ble::SERVICE_UUID_16;

/// Client Characteristic Configuration, the descriptor a subscribe writes.
const CCCD: u16 = 0x2902;

/// The vendor characteristic carrying the timecode, headerless. The only one
/// of the four with a clock in it, and so the only one a round trip is worth
/// taking on — `0dab1280` changes when the box is synced and never otherwise,
/// and `0dab2496` has never changed at all.
const TIMECODE_CHAR: Uuid = Uuid::from_u128(0x0dab_144c_2cb9_11e6_b67b_9e71_128c_ae77);

/// How long to wait for a connect before giving up and going round again.
///
/// `--seconds` is only tested between sessions, so an unbounded connect is a
/// deadline the run cannot honour — and this box does stop answering them
/// after a few dozen reconnects, wedging a capture indefinitely with nothing
/// in the log to say why. Fifteen seconds matches `rediscover`'s give-up,
/// which is the other wait in this loop that could otherwise never end.
const CONNECT_GIVE_UP: Duration = Duration::from_secs(15);

/// The record type an advertised timecode carries in its first header byte.
/// The vendor characteristic sends the same record with the header taken off,
/// so putting one back on is the whole of the decode — see `vendor_timecode`.
const KIND_TIMECODE: u8 = 0x22;

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
    let mut run = Run {
        // Opened before the first connect, so a run that cannot write its
        // samples fails now rather than after minutes of collecting them.
        phase: opt.phase.as_deref().map(Phase::new).transpose()?,
        ..Default::default()
    };
    let mut peripheral = central.peripheral(id).await?;
    loop {
        eprintln!("connecting to {name} …");
        let attempt = match tokio::time::timeout(CONNECT_GIVE_UP, peripheral.connect()).await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err(format!(
                "no answer in {:.0}s",
                CONNECT_GIVE_UP.as_secs_f64()
            )),
        };
        if let Err(e) = attempt {
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

    // A read of anything else costs a connection event and measures nothing:
    // `0dab1280` moves only when the box is synced and `0dab2496` has never
    // moved at all, so timing a round trip on either spends an anchor point to
    // learn what the last one already said.
    let phase_run = run.phase.is_some();
    if phase_run {
        readable.retain(|ch| ch.uuid == TIMECODE_CHAR);
        // Said once, at the top, because the samples it spoils look perfectly
        // ordinary in the file and only the analysis notices.
        if first && !opt.no_subscribe {
            eprintln!(
                "warning: --phase without --no-subscribe. A subscribed read returns\n\
                 a notification rather than a Read Response, so the round trips in\n\
                 this capture will not be round trips. See `Phase`."
            );
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
    let n = run.sessions;
    if let Some(phase) = &mut run.phase {
        phase.session(overall, start, n, "connected");
    }
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
    // Phase mode schedules every tick after the first from `dither`, so
    // the period here only decides how soon the first one comes — and with six
    // seconds of connection to spend it should come at once.
    let period = match phase_run {
        true => Duration::from_millis(1),
        false => Duration::from_millis(opt.poll_ms.max(1)),
    };
    let mut poll = tokio::time::interval(period);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await; // the first tick is immediate, and we just read them
    let polling = opt.poll_ms > 0 || phase_run;
    let mut dead = false;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            note = notifications.next() => {
                // Stamped before anything else touches it: a notification's
                // arrival time is the only thing about it this host knows, and
                // a formatting call between the wire and the clock is delay
                // charged to the device.
                let at = Instant::now();
                let Some(note) = note else {
                    println!("[{:7.3}s] notification stream ended", overall.elapsed().as_secs_f64());
                    break;
                };
                lived = start.elapsed();
                if let Some(phase) = &mut run.phase {
                    phase.one_way("notify", overall, at, vendor_timecode(&note.value));
                }
                println!(
                    "[{:7.3}s] NOTIFY {}  {}{}",
                    overall.elapsed().as_secs_f64(),
                    tail(&note.uuid),
                    hex(&note.value),
                    gloss(&note.value),
                );
                run.digests.entry(note.uuid).or_default().add(&note.value);
            }
            _ = poll.tick(), if polling => {
                for ch in &readable {
                    // The experiment, and all of it: a stamp either side of a
                    // round trip. Nothing between `t0` and the request but the
                    // read itself, and nothing between the response and `t1`.
                    let t0 = Instant::now();
                    let read = peripheral.read(ch).await;
                    let t1 = Instant::now();
                    match read {
                        Ok(bytes) => {
                            if let Some(phase) = &mut run.phase {
                                phase.round_trip(overall, t0, t1, &bytes);
                            }
                            run.polls.entry(ch.uuid).or_default().add(&bytes);
                            lived = start.elapsed();
                            // A phase run reads several times a second and the
                            // timecode is different every time, so the log
                            // this would write is one line per sample and the
                            // file already has them all.
                            if !phase_run {
                                run.note(ch.uuid, &bytes, overall, "READ  ");
                            }
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
                if let Some(phase) = &mut run.phase {
                    let wait = phase.wait();
                    poll.reset_after(wait);
                }
            }
            event = events.next(), if opt.scan => {
                // Same reason as the notification arm: stamp it before
                // deciding whether it is even interesting.
                let at = Instant::now();
                let Some(event) = event else { continue };
                let Some(from) = event_peripheral(&event) else { continue };
                let Some(line) = advert_line(&event) else { continue };
                // Two boxes are usually in range and both advertise the same
                // shape, so an unlabelled log invites reading one box's bytes
                // as the other's. Only the connected one is counted.
                let mine = *from == id;
                if mine {
                    run.adverts += 1;
                    // Only the connected box's, because the offset being
                    // measured is that box's. Another Tentacle on the same
                    // timeline is a second unknown offset, not a second
                    // reading of this one.
                    if let Some(tc) = advert_timecode(&event)
                        && let Some(phase) = &mut run.phase
                    {
                        phase.one_way("advert", overall, at, Some(tc));
                    }
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
    if let Some(phase) = &mut run.phase {
        phase.session(overall, Instant::now(), n, "dropped");
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
    /// Where `--phase` samples go, if it was asked for.
    phase: Option<Phase>,
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
        if let Some(phase) = &self.phase {
            let _ = match &phase.failed {
                // Said plainly, because a truncated capture that looks whole
                // is the one failure mode that would poison the analysis
                // rather than stop it.
                Some(e) => writeln!(
                    out,
                    "  phase: {} sample(s) written, then writing failed: {e}",
                    phase.records,
                ),
                None => writeln!(
                    out,
                    "  phase: {} sample(s), {} round trip(s)",
                    phase.records, phase.reads,
                ),
            };
        }
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

/// The offset measurement: what a round trip gives you that a broadcast cannot.
///
/// Nothing that only listens can say how far the device's clock sits from this
/// host's. A transmit-path constant, a flight time and a stack delay all look
/// identical to a receiver, and a one-way packet carries no way to tell them
/// apart. An ATT read is not one-way — a Read Request goes out and a Read
/// Response comes back — so stamping the host clock either side of one gives
/// the four timestamps NTP works from, with the timecode in the response
/// standing in for the device's own two:
///
/// ```text
///   t0 ──── request ───▶ ┃ device stamps T ┃ ──── response ───▶ t1
/// ```
///
/// Write `a = T - t0` and `b = t1 - T`, and let `θ` be what we want: the
/// device's clock minus this host's. Then `a = d_out + θ` and `b = d_ret - θ`,
/// where `d_out` and `d_ret` are the two legs. Neither leg is knowable on its
/// own — but both are times, so both are at least zero, and that alone is
/// enough:
///
/// ```text
///   θ ≤ a   for every sample,    θ ≥ -b   for every sample
/// ```
///
/// so `min(a)` bounds the offset above and `-min(b)` bounds it below. **That
/// costs no assumption about the two legs being equal**, which matters here
/// because they are conspicuously not: a request handed to the controller
/// waits for the next connection anchor point and a response, already at the
/// device, does not. Halving the round trip — the move that turns NTP's
/// bracket into a single number — would put the answer at the middle of that
/// bracket and silently take the asymmetry on as bias. The bracket is the
/// honest form, and its width is the shortest round trip the run managed.
///
/// Which is why the reads are dithered; see [`dither`].
///
/// # Do not subscribe while doing this
///
/// A read taken while subscribed is not a round trip. On macOS a Read Response
/// and a notification arrive at CoreBluetooth through the same delegate
/// callback, and nothing downstream can tell them apart, so a `read` in flight
/// is resolved by whichever lands first — usually a notification, since the
/// device pushes one every connection event. The value is real and its timing
/// is not: the round trip appears to have taken no time at all.
///
/// It is not subtle once looked for. Over two captures on the same box,
/// counting round trips shorter than one 30 ms connection interval — which a
/// real one cannot be:
///
/// ```text
///   --no-subscribe      0 of   443
///   subscribed      2,833 of 3,388     (83.6%)
/// ```
///
/// and the subscribed capture's shortest "round trip" was 31 µs. So `--phase`
/// wants `--no-subscribe`, and warns when it doesn't get it. A subscribed
/// capture is not wasted — its notifications are genuine, and their arrival
/// times still say what the notification path costs — but its reads cannot
/// bound anything.
///
/// This writes samples and reduces nothing. `analysis/gatt_phase.py` does the
/// arithmetic above, checks for the artefact just described, and has the
/// caveats that go with both.
struct Phase {
    out: LineWriter<File>,
    /// Reads issued, which is what walks the dither.
    reads: u64,
    /// Lines written, for the closing summary to report.
    records: u64,
    /// The first write that failed, if one did. A capture that quietly stopped
    /// recording halfway is worse than one that says it did.
    failed: Option<std::io::Error>,
}

impl Phase {
    fn new(path: &Path) -> Result<Phase> {
        Ok(Phase {
            out: LineWriter::new(File::create(path)?),
            reads: 0,
            records: 0,
            failed: None,
        })
    }

    /// How long to wait before the next read. See [`dither`].
    fn wait(&mut self) -> Duration {
        self.reads = self.reads.wrapping_add(1);
        dither(self.reads)
    }

    /// One round trip: both host stamps and whatever the device said between
    /// them.
    fn round_trip(&mut self, overall: &Instant, t0: Instant, t1: Instant, bytes: &[u8]) {
        let record = format!(
            r#"{{"kind":"read","t0_micros":{},"t1_micros":{},"payload":"{}",{}}}"#,
            t0.saturating_duration_since(*overall).as_micros(),
            t1.saturating_duration_since(*overall).as_micros(),
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            fields(vendor_timecode(bytes)),
        );
        self.emit(&record);
    }

    /// A notification, which has an arrival stamp and no departure one. Not a
    /// round trip and not treated as one: it is here so the same run can say
    /// what the notification path's floor is, which is what bridges this
    /// measurement onto the advertisement path.
    fn one_way(&mut self, kind: &str, overall: &Instant, at: Instant, tc: Option<Timecode>) {
        let record = format!(
            r#"{{"kind":"{kind}","at_micros":{},{}}}"#,
            at.saturating_duration_since(*overall).as_micros(),
            fields(tc),
        );
        self.emit(&record);
    }

    /// A connection opening or closing. The offset is a property of the two
    /// clocks and carries across a reconnect, but the round trips inside one
    /// session share a connection anchor grid and those in the next do not, so
    /// the boundaries have to be on the record.
    fn session(&mut self, overall: &Instant, at: Instant, n: usize, what: &str) {
        let record = format!(
            r#"{{"kind":"session","at_micros":{},"session":{n},"event":"{what}"}}"#,
            at.saturating_duration_since(*overall).as_micros(),
        );
        self.emit(&record);
    }

    fn emit(&mut self, record: &str) {
        if self.failed.is_some() {
            return;
        }
        match writeln!(self.out, "{record}") {
            Ok(()) => self.records += 1,
            Err(e) => self.failed = Some(e),
        }
    }
}

/// How long to wait before the `n`th read, so that `t0` sweeps the connection
/// anchor grid instead of landing on one phase of it.
///
/// A read handed to the controller waits for the next connection anchor before
/// it goes anywhere, so most of what a round trip measures is where `t0`
/// happened to fall in the 30 ms between two of them. That is not a nuisance
/// to be averaged away — the *shortest* round trip is the entire measurement,
/// and it only happens when `t0` lands just before an anchor. Poll on a fixed
/// cadence and `t0` can sit at one phase of that grid for a whole session,
/// putting a floor under the round trip that belongs to the polling and not to
/// the link.
///
/// Stepping by a prime number of microseconds across a span slightly wider
/// than the interval walks every phase of it, since a step sharing no factor
/// with the span visits the whole of it before repeating — and the span is
/// deliberately not the measured 30 ms, so the sweep still covers a whole
/// interval if the real one is a little different.
fn dither(n: u64) -> Duration {
    /// Coprime with `SPAN`, so the sequence visits every microsecond of it
    /// before it repeats.
    const STEP: u64 = 7_919;
    /// A little over the 30 ms connection interval `PROTOCOL.md` measures.
    const SPAN: u64 = 31_000;
    Duration::from_micros(n.wrapping_mul(STEP) % SPAN)
}

/// A timecode's fields as JSON, or the same keys set to null when the payload
/// didn't decode. One shape either way: a reader that has to cope with absent
/// keys copes with them wrongly.
fn fields(tc: Option<Timecode>) -> String {
    match tc {
        Some(tc) => format!(
            r#""fps":{},"hours":{},"minutes":{},"seconds":{},"frames":{},"subframe_micros":{}"#,
            tc.rate.fps,
            tc.hours,
            tc.minutes,
            tc.seconds,
            tc.frames,
            tc.subframe.as_micros(),
        ),
        None => r#""fps":null,"hours":null,"minutes":null,"seconds":null,"frames":null,"subframe_micros":null"#.to_string(),
    }
}

/// A vendor characteristic payload as a timecode.
///
/// `0dab144c` carries the advertisement's `0x22` record with its two header
/// bytes taken off — `PROTOCOL.md` has the byte-by-byte evidence — so putting a
/// header back on and handing it to the advertisement parser is the whole of
/// the decode, and reuses the range checks that keep some other vendor's blob
/// from being read as a clock. Byte 1 is skipped rather than read, so what
/// goes there doesn't matter.
fn vendor_timecode(bytes: &[u8]) -> Option<Timecode> {
    let mut framed = Vec::with_capacity(ble::HEADER + bytes.len());
    framed.extend_from_slice(&[KIND_TIMECODE, 0]);
    framed.extend_from_slice(bytes);
    match ble::parse(&framed)? {
        Advert::Timecode(tc) => Some(tc),
        Advert::Date(_) => None,
    }
}

/// The timecode an advertisement event carries, if it carries one.
fn advert_timecode(event: &CentralEvent) -> Option<Timecode> {
    let CentralEvent::ServiceDataAdvertisement { service_data, .. } = event else {
        return None;
    };
    let data = service_data.get(&uuid_from_u16(ble::SERVICE_UUID_16))?;
    match ble::parse(data)? {
        Advert::Timecode(tc) => Some(tc),
        Advert::Date(_) => None,
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
    fn a_vendor_payload_decodes_as_the_headerless_record_it_is() {
        // PROTOCOL.md's worked example off `0dab144c`: the advertisement's
        // 0x22 record with its two header bytes taken away. Framing it back up
        // wrongly — one byte instead of two, or the wrong record type — moves
        // every field and this is what notices.
        let tc = vendor_timecode(&[0x19, 0x0c, 0x24, 0x33, 0x05, 0x67, 0x51])
            .expect("seven bytes of headerless timecode");
        assert_eq!(tc.rate.fps, 25);
        assert_eq!((tc.hours, tc.minutes, tc.seconds, tc.frames), (12, 36, 51, 5));
        assert_eq!(tc.subframe.as_micros(), 0x6751);
    }

    #[test]
    fn the_other_vendor_characteristics_are_not_read_as_a_clock() {
        // A phase capture reads one characteristic, but the decoder is handed
        // whatever notifies, and the two other vendor characteristics do have
        // bytes in them. Read as a clock they would put nonsense on the
        // timeline the offset is measured from, which is worse than a gap.
        // `0dab2496`, twenty-four zero bytes:
        assert!(vendor_timecode(&[0u8; 24]).is_none());
        // `0dab1280`, Ricki's state record from the sync in PROTOCOL.md:
        let state = [
            0x0d, 0x6c, 0x00, 0x0c, 0x01, 0x00, 0x53, 0x19, 0x00, 0x00, 0x04, 0x09, 0x1a, 0x0b,
            0x1c, 0x28,
        ];
        assert!(vendor_timecode(&state).is_none());
    }

    #[test]
    fn a_payload_that_did_not_decode_still_writes_every_key() {
        // What `fields` claims: one shape whether or not there was a timecode
        // in it. A reader that has to cope with a key being absent copes with
        // it wrongly, and the analysis would silently skip the samples that
        // most need explaining.
        let keys = |json: String| -> Vec<String> {
            json.split(',')
                .map(|pair| pair.split(':').next().unwrap_or_default().to_string())
                .collect()
        };
        let decoded = vendor_timecode(&[0x19, 0x0c, 0x24, 0x33, 0x05, 0x67, 0x51]);
        assert!(decoded.is_some());
        assert_eq!(keys(fields(decoded)), keys(fields(None)));
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
