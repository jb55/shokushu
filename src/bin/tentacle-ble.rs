//! Reads timecode off a Tentacle Sync E over Bluetooth LE, without pairing —
//! the device broadcasts it in its advertisements.
//!
//! Advertisements arrive only once or twice a second, so the live display
//! doesn't wait for them: [`tentacle::freerun`] keeps a local clock anchored to
//! each one and this redraws at [`TICK`], which is what makes the timecode tick
//! smoothly instead of jumping. `--json` is left alone — it emits the readings
//! that actually arrived, and nothing interpolated.
//!
//! Every Tentacle in range gets a line of its own, since each keeps its own
//! clock: two boxes needn't be showing the same timecode, or even running at the
//! same frame rate.
//!
//! When nothing decodes there is nothing to draw, and a blank screen is the one
//! thing this must never be: `0xFDAC` service data whose payload has changed
//! looks exactly like an empty room. So the display says what it is taking in
//! instead — see [`diagnose`].
//!
//! `--raw` turns this back into the reconnaissance tool it started as, dumping
//! advertisement payloads and marking which bytes changed. That's how the
//! layout in [`tentacle::ble`] was worked out, and it's the way to work out
//! anything still unknown — how a 29.97 drop-frame device differs, say.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use clap::Parser;
use futures::stream::StreamExt;
use tentacle::ble::{self, Advert, Date, Timecode, HEADER};
use tentacle::freerun::{FreeRun, Reading};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(version, about = "Read Tentacle Sync E timecode over Bluetooth LE")]
struct Opt {
    /// Dump raw advertisement payloads instead of decoding timecode.
    #[arg(long)]
    raw: bool,

    /// Only look at devices whose name contains this (case-insensitive).
    #[arg(short, long)]
    name: Option<String>,

    /// Stop after this many seconds. 0 runs until interrupted.
    #[arg(short, long, default_value_t = 0)]
    seconds: u64,

    /// Emit one JSON object per advertisement received, instead of a live
    /// display. Only real readings — nothing interpolated.
    #[arg(short, long)]
    json: bool,

    /// With --raw, print every advertisement rather than only changed payloads.
    #[arg(short, long)]
    all: bool,
}

/// What we last saw from one device.
#[derive(Default)]
struct Seen {
    name: Option<String>,
    rssi: Option<i16>,
    date: Option<Date>,
    /// Remaining charge, from the manufacturer record — which rides in the same
    /// advertisement as the timecode but arrives as its own event.
    battery: Option<u8>,
    /// The local clock this device's advertisements anchor.
    clock: FreeRun,
    /// When this device first sent timecode, which is where its line sits.
    /// Something fixed has to decide that: `seen` is a `HashMap`, and iterating
    /// it hands the devices back in a different order on every redraw.
    first_timecode: Option<Instant>,
    manufacturer: HashMap<u16, Vec<u8>>,
    service: HashMap<Uuid, Vec<u8>>,
    adverts: u64,
    /// Service payloads received under `0xFDAC`, and how many of those
    /// [`ble::parse`] turned down. Only interesting when there's nothing to
    /// draw, which is exactly when they're the only thing to go on.
    fdac: u64,
    unparsed: u64,
    /// The last payload that didn't parse. A wire format that has moved is
    /// invisible without the bytes in front of you — this is what put a stop to
    /// the last one.
    unparsed_sample: Option<Vec<u8>>,
}

/// How often to redraw the live display. Comfortably above any frame rate a
/// Tentacle broadcasts, so each frame appears within a tick of when it starts,
/// and far too cheap to be worth tuning.
const TICK: Duration = Duration::from_millis(20);

/// How long a device that's gone quiet keeps its line.
///
/// Past [`tentacle::freerun::HOLDOVER`] a line freezes on the last reading that
/// arrived and counts up, which is worth seeing: reception is bursty and usually
/// comes back. A box switched off ten minutes ago isn't coming back and
/// shouldn't still be holding a line, so the line goes once it has been silent
/// this long. The device stays in `seen`, so if it does return it reappears
/// where it was rather than jumping to the bottom.
const LINGER: Duration = Duration::from_secs(30);

/// How long to give timecode before saying what the scan is actually seeing.
///
/// Long enough that a healthy Tentacle is never accused of silence: adverts
/// arrive around three times a second per device and a fresh reading nearly
/// twice a second, so by this point a box in range has had a dozen chances even
/// allowing for the burst gaps in `PROTOCOL.md`. Short enough that nobody sits
/// watching an empty screen wondering whether to press something.
const GRACE: Duration = Duration::from_secs(3);

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

    eprintln!(
        "adapter state: {:?} — scanning{}{}",
        central.adapter_state().await?,
        match opt.seconds {
            0 => " until interrupted".to_string(),
            n => format!(" for {n}s"),
        },
        match &opt.name {
            Some(n) => format!(", filtering on {n:?}"),
            None => String::new(),
        }
    );

    let mut events = central.events().await?;
    central.start_scan(ScanFilter::default()).await?;

    let tentacle_service = uuid_from_u16(ble::SERVICE_UUID_16);
    let start = Instant::now();
    let deadline = async {
        match opt.seconds {
            0 => std::future::pending::<()>().await,
            n => tokio::time::sleep(Duration::from_secs(n)).await,
        }
    };
    tokio::pin!(deadline);

    let mut seen: HashMap<PeripheralId, Seen> = HashMap::new();
    // How many lines the last redraw left on screen, which is what the next one
    // has to move the cursor back up by.
    let mut drawn = 0usize;
    let mut notice = Notice::new();
    let interpolating = !opt.raw && !opt.json;
    let mut ticker = tokio::time::interval(TICK);
    // Redraws are only worth doing on an even cadence. Catching up on ticks
    // missed while the event loop was busy would bunch several together, all
    // showing the same time.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let event = tokio::select! {
            _ = &mut deadline => break,
            _ = ticker.tick(), if interpolating => {
                render(
                    &mut seen,
                    &mut drawn,
                    Instant::now(),
                    &mut notice,
                    opt.name.as_deref(),
                    start.elapsed(),
                );
                continue;
            }
            event = events.next() => match event {
                Some(e) => e,
                None => break,
            },
        };
        // Stamped here, before the name lookup below can await: an anchor is
        // only as good as the arrival time paired with it.
        let arrived = Instant::now();

        let Some(id) = event_peripheral(&event) else {
            continue;
        };
        learn_name(&central, &mut seen, &id).await;

        let entry = seen.get_mut(&id).unwrap();
        if !name_matches(&opt, entry) {
            continue;
        }
        entry.adverts += 1;

        if let CentralEvent::RssiUpdate { rssi, .. } = event {
            entry.rssi = Some(rssi);
            continue;
        }

        if opt.raw {
            report_raw(&opt, entry, &id, start.elapsed().as_secs_f64(), event);
        } else {
            decode(&opt, entry, &tentacle_service, event, arrived);
        }
    }

    central.stop_scan().await?;
    // Every drawn line ends in a newline, so the cursor is already sitting on a
    // fresh one below the display; there is nothing to add to leave it there.
    // A diagnostic is the exception — it's deliberately left un-terminated so
    // it can be rewritten in place, so close it off rather than let the shell
    // prompt land on top of the last thing we said.
    notice.finish();
    Ok(())
}

/// Takes in one advertisement.
///
/// In `--json` this prints the reading; otherwise it only anchors that device's
/// clock, and [`render`] does the drawing on its own schedule.
fn decode(
    opt: &Opt,
    seen: &mut Seen,
    tentacle_service: &Uuid,
    event: CentralEvent,
    arrived: Instant,
) {
    if let CentralEvent::ManufacturerDataAdvertisement {
        manufacturer_data, ..
    } = &event
    {
        // Charge, not time: hold onto it and let the timecode below do the
        // printing. It changes about once every twenty minutes, so there's no
        // sense in it driving anything.
        if let Some(bytes) = manufacturer_data.get(&ble::COMPANY_ID)
            && let Some(status) = ble::parse_manufacturer(bytes)
        {
            seen.battery = Some(status.battery_percent);
        }
        return;
    }

    let CentralEvent::ServiceDataAdvertisement { service_data, .. } = event else {
        return;
    };
    let Some(payload) = service_data.get(tentacle_service) else {
        return;
    };
    seen.fdac += 1;
    let Some(advert) = ble::parse(payload) else {
        // Not a reading, but the most useful thing there is to say when no
        // reading ever comes: a Tentacle is right here and we can't read it.
        seen.unparsed += 1;
        seen.unparsed_sample = Some(payload.clone());
        return;
    };

    match advert {
        Advert::Date(date) => {
            // The date comes round far more rarely than the timecode, so hold
            // onto it and show it alongside.
            seen.date = Some(date);
        }
        Advert::Timecode(tc) => {
            if opt.json {
                println!(
                    r#"{{"timecode":"{tc}","hours":{},"minutes":{},"seconds":{},"frames":{},"subframe_micros":{},"fps":{},"device":"{}","date":{},"rssi":{},"battery_percent":{}}}"#,
                    tc.hours,
                    tc.minutes,
                    tc.seconds,
                    tc.frames,
                    tc.subframe_micros,
                    tc.fps,
                    seen.name.as_deref().unwrap_or("<unnamed>"),
                    seen.date.map_or("null".into(), |d| format!("\"{d}\"")),
                    seen.rssi.map_or("null".to_string(), |r| r.to_string()),
                    seen.battery.map_or("null".to_string(), |b| b.to_string()),
                );
            } else {
                seen.first_timecode.get_or_insert(arrived);
                seen.clock.anchor(&tc, arrived);
            }
        }
    }
}

/// One device's line, before it's laid out. The name column is padded to the
/// widest name on screen, so every row has to be in hand before any one of them
/// can be formatted.
struct Row {
    /// First timecode, then id to break a tie: where this line sits, and fixed
    /// for as long as the device keeps it.
    order: (Instant, String),
    tc: Timecode,
    name: String,
    date: Option<Date>,
    rssi: Option<i16>,
    battery: Option<u8>,
    note: String,
}

/// Draws a line per device from that device's free-running clock, so each ticks
/// between its own advertisements instead of only when one lands — and two boxes
/// in range don't fight over a single line.
fn render(
    seen: &mut HashMap<PeripheralId, Seen>,
    drawn: &mut usize,
    now: Instant,
    notice: &mut Notice,
    filter: Option<&str>,
    elapsed: Duration,
) {
    let mut rows = Vec::new();

    for (id, entry) in seen.iter_mut() {
        // No anchor yet means no clock to sample, which is also the filter that
        // keeps the display to Tentacles: `seen` holds every peripheral the
        // adapter noticed, and most of them never send timecode.
        let (Some(first), Some(reading)) = (entry.first_timecode, entry.clock.sample(now)) else {
            continue;
        };

        // Off the air, freeze on the last reading that actually arrived and say
        // how long ago, rather than carrying on and making timecode up.
        let (tc, note) = match reading {
            Reading::Running(tc) => (tc, String::new()),
            Reading::Lost { last, since } => {
                if since > LINGER {
                    continue;
                }
                (
                    last,
                    format!("   no signal for {:.1}s", since.as_secs_f64()),
                )
            }
        };

        rows.push(Row {
            order: (first, short_id(id)),
            tc,
            name: entry.name.clone().unwrap_or_else(|| "<unnamed>".into()),
            date: entry.date,
            rssi: entry.rssi,
            battery: entry.battery,
            note,
        });
    }

    let lines = lay_out(rows);

    // The diagnostic and the display want the same line, so only one of them
    // may hold it. Order matters both ways round: the line has to be given up
    // before the display draws over it, and claimed only after a redraw that
    // blanks vacated lines has finished moving the cursor about.
    if !lines.is_empty() {
        notice.clear();
    }
    print!("{}", redraw(&lines, *drawn));
    *drawn = lines.len();
    let _ = std::io::stdout().flush();

    if lines.is_empty() && elapsed >= GRACE {
        notice.show(diagnose(&census(seen), filter));
    }
}

/// What the scan has taken in, gathered across every device.
///
/// Consulted only when the display has nothing on it, to tell apart three
/// failures that otherwise look identical — nothing in range, something in
/// range whose payload no longer decodes, and a scan delivering no events at
/// all. The user has been in the middle one of those, staring at the first.
#[derive(Default, Debug, PartialEq, Eq)]
struct Census {
    /// Peripherals the adapter reported anything about.
    devices: usize,
    /// ... of those, the ones that got past `--name`.
    matched: usize,
    /// ... of those, the ones that sent service data under `0xFDAC`.
    advertisers: usize,
    /// `0xFDAC` payloads received from them in total.
    payloads: u64,
    /// ... of those, the ones [`ble::parse`] turned down.
    unparsed: u64,
    /// The last payload that didn't parse, and who sent it.
    sample: Option<(String, Vec<u8>)>,
}

fn census(seen: &HashMap<PeripheralId, Seen>) -> Census {
    // `adverts` is only incremented once a device is past the name filter, so
    // it's what separates "in range" from "in range and being looked at".
    let matched: Vec<&Seen> = seen.values().filter(|entry| entry.adverts > 0).collect();
    survey(seen.len(), &matched)
}

/// The counting half, split from [`census`] so it can be tested: a
/// `PeripheralId` can only be minted by the platform, and none of this cares
/// which device is which beyond having a name to print.
fn survey(devices: usize, matched: &[&Seen]) -> Census {
    Census {
        devices,
        matched: matched.len(),
        advertisers: matched.iter().filter(|entry| entry.fdac > 0).count(),
        payloads: matched.iter().map(|entry| entry.fdac).sum(),
        unparsed: matched.iter().map(|entry| entry.unparsed).sum(),
        sample: matched
            .iter()
            // Most failures first, name breaking a tie: `seen` hands its values
            // back in a different order every redraw, and a diagnostic that
            // blames a different device each time is one nobody believes. A
            // device with failures always outranks one without, and only a
            // device with failures has a sample to offer.
            .max_by_key(|entry| (entry.unparsed, entry.name.as_deref()))
            .and_then(|entry| Some((name_of(entry).to_string(), entry.unparsed_sample.clone()?))),
    }
}

/// A diagnostic, and a key for which failure it describes.
///
/// The key exists because the text moves on its own: the device count climbs as
/// the adapter notices more of the room, and an unreadable payload's timecode
/// bytes change with every advertisement. Keyed on the text, a redirected
/// stderr would collect a line per passing pair of headphones and a line per
/// packet — the scrolling log this is supposed to replace. A terminal rewrites
/// in place and can afford to stay current; a file keys on this instead. See
/// [`shape_of`] for what counts as a different unreadable payload.
struct Diagnosis {
    key: String,
    text: String,
}

impl Diagnosis {
    fn new(key: &str, text: String) -> Diagnosis {
        Diagnosis {
            key: key.to_string(),
            text,
        }
    }
}

/// One line saying what the scan is taking in, for when none of it decodes.
///
/// Ordered most specific first. Every branch has to name something the reader
/// can act on, because the alternative — which is what this replaced — is a
/// blank screen that means all of them at once.
fn diagnose(census: &Census, filter: Option<&str>) -> Diagnosis {
    let Census {
        devices,
        matched,
        advertisers,
        payloads,
        unparsed,
        sample,
    } = census;

    if *devices == 0 {
        return Diagnosis::new(
            "silent-scan",
            "no timecode: not one BLE advertisement of any kind has arrived, so nothing is \
             reaching this process — suspect the scan, not the Tentacles"
                .to_string(),
        );
    }
    if *matched == 0 {
        // Nothing can fail a filter that isn't there, so the `None` arm is a
        // shape the event loop can't produce. Say something true anyway rather
        // than assert a filter that was never passed.
        return match filter {
            Some(want) => Diagnosis::new(
                "filtered-out",
                format!(
                    "no timecode: {} in range, none named like {want:?} — try again without \
                     --name",
                    tally(*devices, "BLE device")
                ),
            ),
            None => Diagnosis::new(
                "nothing-advertising",
                format!(
                    "no timecode: {} in range, none of which has advertised anything",
                    tally(*devices, "BLE device")
                ),
            ),
        };
    }
    if let Some((name, bytes)) = sample {
        let key = format!("unparsed:{}", shape_of(bytes));
        let bytes = hex(bytes);
        return Diagnosis::new(
            &key,
            format!(
                "no timecode: {} advertising 0x{:04X}, but {unparsed} of {payloads} payloads \
                 did not decode — {name} last sent {bytes} (--raw -a dumps them all; see \
                 PROTOCOL.md)",
                tally(*advertisers, "device"),
                ble::SERVICE_UUID_16,
            ),
        );
    }
    if *advertisers > 0 {
        return Diagnosis::new(
            "dates-only",
            format!(
                "no timecode: {} advertising 0x{:04X} and all {payloads} payloads decoded, but \
                 none has carried timecode yet — dates only so far",
                tally(*advertisers, "device"),
                ble::SERVICE_UUID_16,
            ),
        );
    }
    Diagnosis::new(
        "no-tentacle",
        format!(
            "no timecode: {} in range, none advertising 0x{:04X} — no Tentacle here",
            tally(*matched, "BLE device"),
            ble::SERVICE_UUID_16,
        ),
    )
}

/// What makes one unreadable payload structurally different from another: the
/// flags byte and the length.
///
/// Deliberately not the whole payload, and deliberately not the record type
/// either. A timecode record's data bytes change with every advertisement, so
/// keying on those would put a line in a redirected log two or three times a
/// second; and a device alternates between timecode and date records in
/// perfectly normal operation, so keying on the record type makes a single
/// format failure alternate between two lines forever. What actually moved when
/// this broke was the header — byte 1 — with the size staying put, and that is
/// what a second line is worth reporting for.
fn shape_of(payload: &[u8]) -> String {
    let flags = payload.get(HEADER - 1).copied().unwrap_or_default();
    format!("{flags:02x}/{}", payload.len())
}

/// `1 device` but `2 devices`, so a message about a bug doesn't read like one.
fn tally(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}

/// Owns one line of diagnostic on stderr.
///
/// The display owns stdout and redraws in place, so a diagnostic that scrolls
/// past it — or worse, is still sitting on the line the display wants — is
/// worse than none at all. This keeps at most one line, rewrites it only when
/// the text changes, and gets out of the way the moment there's timecode.
///
/// On a terminal that's a single line rewritten in place. Redirected to a file
/// there's no cursor to move, so each distinct message becomes a line of its
/// own — which is also what makes the rewrite-on-change rule matter rather than
/// being an optimisation: without it a redirected stderr would collect fifty
/// identical lines a second for as long as the box stayed quiet.
struct Notice {
    tty: bool,
    shown: Option<String>,
    key: Option<String>,
}

impl Notice {
    fn new() -> Notice {
        Notice {
            tty: std::io::stderr().is_terminal(),
            shown: None,
            key: None,
        }
    }

    /// Puts a diagnosis on screen, or leaves it be if it's already said.
    ///
    /// On a terminal that's per changed word, since rewriting a line in place
    /// costs nothing and keeping the counts current is worth something. In a
    /// file it's per changed [`Diagnosis::key`] — same failure, same line, no
    /// matter how long it lasts.
    fn show(&mut self, diagnosis: Diagnosis) {
        let Diagnosis { key, text } = diagnosis;
        if self.tty {
            if self.shown.as_deref() == Some(text.as_str()) {
                return;
            }
            eprint!("\r{text}\x1b[K");
            let _ = std::io::stderr().flush();
        } else {
            if self.key.as_deref() == Some(key.as_str()) {
                return;
            }
            eprintln!("{text}");
        }
        self.shown = Some(text);
        self.key = Some(key);
    }

    /// Gives the line back, for when the display has something to put there.
    fn clear(&mut self) {
        self.key = None;
        if self.shown.take().is_some() && self.tty {
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        }
    }

    /// Leaves the cursor below the message rather than on it, at exit.
    fn finish(&mut self) {
        if self.shown.is_some() && self.tty {
            eprintln!();
        }
    }
}

/// Puts the rows in a fixed order and formats each one into a line.
///
/// The order has to come out the same on every redraw. Rows arrive in
/// `HashMap` order, which is randomised per iteration, so drawing them as they
/// come would have the devices swapping places fifty times a second — worse to
/// look at than the one line this replaced.
fn lay_out(mut rows: Vec<Row>) -> Vec<String> {
    rows.sort_by(|a, b| a.order.cmp(&b.order));
    let name_width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);

    rows.iter()
        .map(|r| {
            format!(
                "  {}{:<3}   {:>3} fps   {:<name_width$}{}{}{}{}",
                r.tc,
                tenth(r.tc.subframe_fraction()),
                r.tc.fps,
                r.name,
                r.date.map_or(String::new(), |d| format!("   {d}")),
                r.rssi.map_or(String::new(), |v| format!("   {v} dBm")),
                // Right-aligned, so 100% and 7% keep the note in one column.
                r.battery.map_or(String::new(), |v| format!("   {v:>3}%")),
                r.note,
            )
        })
        .collect()
}

/// The escape sequence that replaces the `previous` lines on screen with these.
///
/// Every line ends in a newline, so the cursor finishes on a fresh line below
/// the display — where it wants to be left at exit, and where the next redraw
/// comes back up from. It moves up by what was drawn last time rather than by
/// what's about to be drawn: a second box coming into range extends the display
/// downwards, and a device dropping off has to have its line blanked before the
/// cursor can come back to sit under the ones that remain.
fn redraw(lines: &[String], previous: usize) -> String {
    if lines.is_empty() && previous == 0 {
        return String::new();
    }

    let mut out = String::new();
    if previous > 0 {
        out.push_str(&format!("\x1b[{previous}A"));
    }
    out.push('\r');
    for line in lines {
        out.push_str(line);
        out.push_str("\x1b[K\n");
    }

    let stale = previous.saturating_sub(lines.len());
    for _ in 0..stale {
        out.push_str("\x1b[K\n");
    }
    if stale > 0 {
        out.push_str(&format!("\x1b[{stale}A"));
    }
    out
}

/// The sub-frame position as a single digit, ".n" of the way into the frame.
///
/// Truncated rather than rounded, because rounding turns the last twentieth of a
/// frame into a "1.0" that reads as part of the frame number — which barely
/// showed when this only drew on arriving packets, and showed constantly once it
/// drew at the frame rate. A received reading can also sit a little past the end
/// of the frame it names (see [`tentacle::ble`]), so clamp rather than widen.
fn tenth(fraction: f64) -> String {
    format!(".{}", (fraction.clamp(0.0, 0.999) * 10.0) as u8)
}

/// Dumps a payload, with a caret under every byte that changed since last time.
fn report_raw(opt: &Opt, seen: &mut Seen, id: &PeripheralId, elapsed: f64, event: CentralEvent) {
    let label = format!(
        "{} [{}]",
        seen.name.clone().unwrap_or_else(|| "<unnamed>".into()),
        short_id(id)
    );

    match &event {
        CentralEvent::ManufacturerDataAdvertisement {
            manufacturer_data, ..
        } => {
            for (company, bytes) in manufacturer_data {
                let previous = seen.manufacturer.get(company).cloned();
                if opt.all || previous.as_deref() != Some(bytes) {
                    print_payload(
                        elapsed,
                        &label,
                        &format!("mfr 0x{company:04x}"),
                        previous.as_deref(),
                        bytes,
                    );
                }
                seen.manufacturer.insert(*company, bytes.clone());
            }
        }
        CentralEvent::ServiceDataAdvertisement { service_data, .. } => {
            for (uuid, bytes) in service_data {
                let previous = seen.service.get(uuid).cloned();
                if opt.all || previous.as_deref() != Some(bytes) {
                    print_payload(
                        elapsed,
                        &label,
                        &format!("svc {uuid}"),
                        previous.as_deref(),
                        bytes,
                    );
                }
                seen.service.insert(*uuid, bytes.clone());
            }
        }
        CentralEvent::DeviceDiscovered(_) => {
            println!("[{elapsed:7.3}s] {label}  discovered");
        }
        _ => {}
    }
}

fn print_payload(elapsed: f64, label: &str, kind: &str, previous: Option<&[u8]>, bytes: &[u8]) {
    println!("[{elapsed:7.3}s] {label}\n    {kind}  {}", hex(bytes));
    let Some(previous) = previous else { return };
    let marks: String = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if previous.get(i) == Some(b) {
                "   "
            } else {
                "^^ "
            }
        })
        .collect();
    if marks.contains('^') {
        println!("    {}  {}", " ".repeat(kind.len()), marks.trim_end());
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

/// Names only arrive through a properties lookup, so fetch each device's once.
async fn learn_name(central: &Adapter, seen: &mut HashMap<PeripheralId, Seen>, id: &PeripheralId) {
    let entry = seen.entry(id.clone()).or_default();
    if entry.name.is_some() {
        return;
    }
    if let Ok(peripheral) = central.peripheral(id).await
        && let Ok(Some(props)) = peripheral.properties().await
    {
        entry.name = props.local_name.or(props.advertisement_name);
        entry.rssi = entry.rssi.or(props.rssi);
    }
}

/// What to call a device that never answered a properties lookup.
fn name_of(seen: &Seen) -> &str {
    seen.name.as_deref().unwrap_or("<unnamed>")
}

fn name_matches(opt: &Opt, seen: &Seen) -> bool {
    match &opt.name {
        None => true,
        Some(want) => seen
            .name
            .as_deref()
            .is_some_and(|n| n.to_lowercase().contains(&want.to_lowercase())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x} "))
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn short_id(id: &PeripheralId) -> String {
    let s = id.to_string();
    s.rsplit(':').next().unwrap_or(&s).chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(order: (Instant, &str), name: &str) -> Row {
        Row {
            order: (order.0, order.1.into()),
            tc: Timecode {
                fps: 25,
                hours: 9,
                minutes: 44,
                seconds: 22,
                frames: 13,
                subframe_micros: 12_000,
            },
            name: name.into(),
            date: None,
            rssi: Some(-46),
            battery: Some(97),
            note: String::new(),
        }
    }

    #[test]
    fn nothing_in_range_draws_nothing() {
        assert_eq!(redraw(&[], 0), "");
    }

    #[test]
    fn the_first_draw_leaves_the_cursor_below_the_lines() {
        assert_eq!(
            redraw(&["a".into(), "b".into()], 0),
            "\ra\x1b[K\nb\x1b[K\n"
        );
    }

    #[test]
    fn a_redraw_comes_up_by_what_was_drawn_last_time() {
        // Two lines on screen and three to draw: come up two, and the third
        // extends the display downwards.
        assert_eq!(
            redraw(&["a".into(), "b".into(), "c".into()], 2),
            "\x1b[2A\ra\x1b[K\nb\x1b[K\nc\x1b[K\n"
        );
    }

    #[test]
    fn a_device_dropping_off_has_its_line_blanked() {
        // Three on screen and one left: the two it vacated are cleared rather
        // than left frozen, and the cursor comes back under the survivor.
        assert_eq!(
            redraw(&["a".into()], 3),
            "\x1b[3A\ra\x1b[K\n\x1b[K\n\x1b[K\n\x1b[2A"
        );
    }

    #[test]
    fn lines_keep_their_order_however_the_map_hands_them_over() {
        let early = Instant::now();
        let late = early + Duration::from_secs(1);
        let ricki = || row((early, "aabbccdd"), "Ricki");
        let bob = || row((late, "00112233"), "Bob");

        let forwards = lay_out(vec![ricki(), bob()]);
        let backwards = lay_out(vec![bob(), ricki()]);

        assert_eq!(forwards, backwards);
        assert!(forwards[0].contains("Ricki"), "{:?}", forwards);
        assert!(forwards[1].contains("Bob"), "{:?}", forwards);
    }

    #[test]
    fn two_devices_that_start_together_still_order_the_same_way() {
        // Instants can tie; the id breaks it, so the order is still fixed.
        let at = Instant::now();
        let one = || row((at, "00112233"), "Bob");
        let two = || row((at, "aabbccdd"), "Ricki");

        assert_eq!(lay_out(vec![one(), two()]), lay_out(vec![two(), one()]));
    }

    #[test]
    fn the_battery_column_keeps_its_width_at_every_charge() {
        // 100% and 7% are three characters apart written plainly, which would
        // shunt the "no signal" note sideways between one device and the next.
        let at = Instant::now();
        let mut full = row((at, "00112233"), "Bob");
        full.battery = Some(100);
        let mut low = row((at + Duration::from_secs(1), "aabbccdd"), "Ricki");
        low.name = "Bob".into();
        low.battery = Some(7);

        let lines = lay_out(vec![full, low]);
        assert!(lines[0].contains("100%"), "{:?}", lines[0]);
        assert!(lines[1].contains("  7%"), "{:?}", lines[1]);
        assert_eq!(lines[0].len(), lines[1].len());
    }

    #[test]
    fn a_device_that_has_not_sent_its_battery_yet_leaves_the_column_out() {
        // The manufacturer record arrives as its own event, so a device can be
        // showing timecode before any charge is known. Better a missing column
        // than a made-up number.
        let at = Instant::now();
        let mut row = row((at, "00112233"), "Bob");
        row.battery = None;
        assert!(!lay_out(vec![row])[0].contains('%'));
    }

    /// What [`diagnose`] would say, for the cases that only care about that.
    fn text_of(census: &Census, filter: Option<&str>) -> String {
        diagnose(census, filter).text
    }

    /// A device the scan has noticed, with whatever it has sent so far.
    fn device(name: &str, fdac: u64, unparsed: u64) -> Seen {
        Seen {
            name: Some(name.into()),
            adverts: 1,
            fdac,
            unparsed,
            unparsed_sample: (unparsed > 0)
                .then(|| vec![0x22, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6]),
            ..Default::default()
        }
    }

    #[test]
    fn a_scan_delivering_nothing_says_so() {
        // The one case where the tool itself is the suspect: not even a passing
        // phone or a pair of headphones has been seen.
        let said = text_of(&survey(0, &[]), None);
        assert!(said.contains("not one BLE advertisement"), "{said}");
        assert!(said.contains("suspect the scan"), "{said}");
    }

    #[test]
    fn a_filter_that_matches_nothing_blames_the_filter() {
        // 40 devices in range and none of them looked at, which is a --name
        // typo far more often than it's an absent device.
        let said = text_of(&survey(40, &[]), Some("rikki"));
        assert!(said.contains("40 BLE devices"), "{said}");
        assert!(said.contains("\"rikki\""), "{said}");
        assert!(said.contains("without --name"), "{said}");
    }

    #[test]
    fn a_room_with_no_tentacle_in_it_says_that() {
        let phone = device("someone's phone", 0, 0);
        let watch = device("<unnamed>", 0, 0);
        let said = text_of(&survey(2, &[&phone, &watch]), None);
        assert!(said.contains("2 BLE devices in range"), "{said}");
        assert!(said.contains("none advertising 0xFDAC"), "{said}");
        assert!(said.contains("no Tentacle here"), "{said}");
    }

    #[test]
    fn a_payload_that_stopped_decoding_is_reported_with_its_bytes() {
        // The failure the user actually hit: two Tentacles right there, every
        // advertisement rejected, and — before this — a blank screen that said
        // exactly as much as an empty room would have.
        let ricki = device("Ricki", 160, 160);
        let liliana = device("Liliana", 151, 151);
        let said = text_of(&survey(72, &[&ricki, &liliana]), None);

        assert!(said.contains("2 devices advertising 0xFDAC"), "{said}");
        assert!(said.contains("311 of 311 payloads did not decode"), "{said}");
        // The bytes are the whole point: a changed wire format can't be worked
        // out from a count of failures.
        assert!(said.contains("22 7d 19 0b 25 28 15 5f c6"), "{said}");
        assert!(said.contains("--raw -a"), "{said}");
    }

    #[test]
    fn a_ticking_timecode_is_not_a_new_failure_every_packet() {
        // The data bytes of a timecode record change with every advertisement.
        // Keyed on those, a redirected log would take a line two or three times
        // a second — so the key must look only at the structural bytes.
        let first = [0x22, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];
        let later = [0x22, 0x7d, 0x19, 0x0b, 0x38, 0x21, 0x11, 0xa0, 0x2e];
        assert_eq!(shape_of(&first), shape_of(&later));

        // Nor is a date record, which a healthy device interleaves with its
        // timecode: one broken format must not alternate between two lines.
        let date = [0x42, 0x7d, 0x00, 0x26, 0x09, 0x04, 0x02, 0xa1, 0x00];
        assert_eq!(shape_of(&first), shape_of(&date));

        // A flags byte or a length that moves is a genuinely different payload,
        // and worth a line of its own — that being the change that broke this.
        let reflagged = [0x22, 0x7e, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];
        assert_ne!(shape_of(&first), shape_of(&reflagged));
        assert_ne!(shape_of(&first), shape_of(&first[..8]));
    }

    #[test]
    fn one_device_reads_as_singular() {
        let ricki = device("Ricki", 9, 9);
        let said = text_of(&survey(1, &[&ricki]), None);
        assert!(said.contains("1 device advertising"), "{said}");
        assert!(!said.contains("1 devices"), "{said}");
    }

    #[test]
    fn dates_arriving_without_timecode_is_its_own_case() {
        // Parsing fine and still nothing to show. Worth distinguishing: it
        // means the timecode record specifically is the thing that moved.
        let ricki = device("Ricki", 4, 0);
        let said = text_of(&survey(3, &[&ricki]), None);
        assert!(said.contains("all 4 payloads decoded"), "{said}");
        assert!(said.contains("none has carried timecode"), "{said}");
    }

    #[test]
    fn the_same_device_is_blamed_however_the_map_hands_them_over() {
        // `seen` iterates in a different order every redraw. A diagnostic that
        // named a different box each time would be worse than none, so the
        // worst offender wins and the name breaks a tie.
        let quiet = device("Aaa", 300, 1);
        let loud = device("Zzz", 300, 200);
        let forwards = survey(2, &[&quiet, &loud]);
        let backwards = survey(2, &[&loud, &quiet]);

        assert_eq!(forwards, backwards);
        assert_eq!(forwards.sample.unwrap().0, "Zzz");
    }

    #[test]
    fn a_device_with_no_failures_is_never_blamed_for_them() {
        // A healthy box whose name happens to sort last must not be picked as
        // the offender just for being last.
        let broken = device("Aaa", 10, 10);
        let healthy = device("Zzz", 10, 0);
        let census = survey(2, &[&healthy, &broken]);

        assert_eq!(census.unparsed, 10);
        assert_eq!(census.sample.unwrap().0, "Aaa");
    }

    #[test]
    fn devices_below_the_name_filter_are_counted_but_not_surveyed() {
        // `census` filters on `adverts`, so a device the loop skipped still
        // shows in the total. Confirm the split survives into the message.
        let census = survey(9, &[]);
        assert_eq!(census.devices, 9);
        assert_eq!(census.matched, 0);
        assert_eq!(census.payloads, 0);
    }

    #[test]
    fn a_notice_only_writes_when_the_words_change() {
        // Redirected to a file there is no cursor to rewrite, so an unchanged
        // message must not be re-emitted — the tick is 20 ms and the quiet case
        // can last minutes.
        let mut notice = Notice { tty: false, shown: None, key: None };

        // A count climbing under an unchanged key must not re-emit: the device
        // count ticks up for as long as the adapter keeps noticing the room.
        notice.show(Diagnosis::new("no-tentacle", "29 devices".to_string()));
        assert_eq!(notice.shown.as_deref(), Some("29 devices"));
        notice.show(Diagnosis::new("no-tentacle", "37 devices".to_string()));
        assert_eq!(notice.shown.as_deref(), Some("29 devices"));

        // A different failure is a different line.
        notice.show(Diagnosis::new("unparsed:7d/9", "bytes moved".to_string()));
        assert_eq!(notice.shown.as_deref(), Some("bytes moved"));
        // Including the same failure with a different payload shape, since a
        // wire format that moves twice is worth saying twice.
        notice.show(Diagnosis::new("unparsed:7e/9", "moved again".to_string()));
        assert_eq!(notice.shown.as_deref(), Some("moved again"));

        // And it gives the line back when the display wants it.
        notice.clear();
        assert_eq!(notice.shown, None);
        assert_eq!(notice.key, None);
    }

    #[test]
    fn the_name_column_is_padded_to_the_widest_name() {
        let at = Instant::now();
        let lines = lay_out(vec![
            row((at, "00112233"), "Bob"),
            row((at + Duration::from_secs(1), "aabbccdd"), "Ricki"),
        ]);

        // Identical but for the name, so equal length means what follows the
        // name lines up between the two.
        assert_eq!(lines[0].len(), lines[1].len());
    }
}
