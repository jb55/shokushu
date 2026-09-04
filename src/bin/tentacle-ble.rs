//! Reads timecode off a Tentacle Sync E over Bluetooth LE, without pairing —
//! the device broadcasts it in its advertisements.
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
use tentacle::ble::{self, Advert, Date};
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

    /// Emit one JSON object per reading instead of a live display.
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
    manufacturer: HashMap<u16, Vec<u8>>,
    service: HashMap<Uuid, Vec<u8>>,
    adverts: u64,
}

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

    loop {
        let event = tokio::select! {
            _ = &mut deadline => break,
            event = events.next() => match event {
                Some(e) => e,
                None => break,
            },
        };

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
            decode(&opt, entry, &tentacle_service, event);
        }
    }

    central.stop_scan().await?;
    if !opt.raw && !opt.json {
        println!();
    }
    Ok(())
}

/// Decodes a Tentacle advertisement and shows the timecode it carries.
fn decode(opt: &Opt, seen: &mut Seen, tentacle_service: &Uuid, event: CentralEvent) {
    let CentralEvent::ServiceDataAdvertisement { service_data, .. } = event else {
        return;
    };
    let Some(payload) = service_data.get(tentacle_service) else {
        return;
    };
    let Some(advert) = ble::parse(payload) else {
        return;
    };

    let name = seen.name.as_deref().unwrap_or("<unnamed>");
    match advert {
        Advert::Date(date) => {
            // The date comes round far more rarely than the timecode, so hold
            // onto it and show it alongside.
            seen.date = Some(date);
        }
        Advert::Timecode(tc) => {
            if opt.json {
                println!(
                    r#"{{"timecode":"{tc}","hours":{},"minutes":{},"seconds":{},"frames":{},"subframe":{:.4},"fps":{},"device":"{name}","date":{},"rssi":{}}}"#,
                    tc.hours,
                    tc.minutes,
                    tc.seconds,
                    tc.frames,
                    tc.subframe_fraction(),
                    tc.fps,
                    seen.date.map_or("null".into(), |d| format!("\"{d}\"")),
                    seen.rssi.map_or("null".to_string(), |r| r.to_string()),
                );
            } else {
                print!(
                    "\r  {tc}{:<3}   {:>3} fps   {name}{}{}   \x1b[K",
                    // The sub-frame position, as ".n" of the way into the frame.
                    format!("{:.1}", tc.subframe_fraction()).trim_start_matches('0'),
                    tc.fps,
                    seen.date.map_or(String::new(), |d| format!("   {d}")),
                    seen.rssi.map_or(String::new(), |r| format!("   {r} dBm")),
                );
                let _ = std::io::stdout().flush();
            }
        }
    }
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
