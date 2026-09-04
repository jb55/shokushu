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
//! `--raw` turns this back into the reconnaissance tool it started as, dumping
//! advertisement payloads and marking which bytes changed. That's how the
//! layout in [`tentacle::ble`] was worked out, and it's the way to work out
//! anything still unknown — how a 29.97 drop-frame device differs, say.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use clap::Parser;
use futures::stream::StreamExt;
use tentacle::ble::{self, Advert, Date, Timecode};
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
    /// The local clock this device's advertisements anchor.
    clock: FreeRun,
    /// When this device first sent timecode, which is where its line sits.
    /// Something fixed has to decide that: `seen` is a `HashMap`, and iterating
    /// it hands the devices back in a different order on every redraw.
    first_timecode: Option<Instant>,
    manufacturer: HashMap<u16, Vec<u8>>,
    service: HashMap<Uuid, Vec<u8>>,
    adverts: u64,
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
                render(&mut seen, &mut drawn, Instant::now());
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
    let CentralEvent::ServiceDataAdvertisement { service_data, .. } = event else {
        return;
    };
    let Some(payload) = service_data.get(tentacle_service) else {
        return;
    };
    let Some(advert) = ble::parse(payload) else {
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
                    r#"{{"timecode":"{tc}","hours":{},"minutes":{},"seconds":{},"frames":{},"subframe_micros":{},"fps":{},"device":"{}","date":{},"rssi":{}}}"#,
                    tc.hours,
                    tc.minutes,
                    tc.seconds,
                    tc.frames,
                    tc.subframe_micros,
                    tc.fps,
                    seen.name.as_deref().unwrap_or("<unnamed>"),
                    seen.date.map_or("null".into(), |d| format!("\"{d}\"")),
                    seen.rssi.map_or("null".to_string(), |r| r.to_string()),
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
    note: String,
}

/// Draws a line per device from that device's free-running clock, so each ticks
/// between its own advertisements instead of only when one lands — and two boxes
/// in range don't fight over a single line.
fn render(seen: &mut HashMap<PeripheralId, Seen>, drawn: &mut usize, now: Instant) {
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
            note,
        });
    }

    let lines = lay_out(rows);
    print!("{}", redraw(&lines, *drawn));
    *drawn = lines.len();
    let _ = std::io::stdout().flush();
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
                "  {}{:<3}   {:>3} fps   {:<name_width$}{}{}{}",
                r.tc,
                tenth(r.tc.subframe_fraction()),
                r.tc.fps,
                r.name,
                r.date.map_or(String::new(), |d| format!("   {d}")),
                r.rssi.map_or(String::new(), |v| format!("   {v} dBm")),
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
