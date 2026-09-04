//! One-shot GATT probe: connect to each Tentacle in range, list what its
//! services expose, and read the standard battery and device-information
//! characteristics.
//!
//! This is the opposite of what `tentacle-ble` does — it pairs nothing but it
//! does open a connection, which can disturb advertising. It exists to answer
//! whether the battery level is available anywhere other than the manufacturer
//! advertisement, and is not part of the scanner.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Manager, PeripheralId};
use futures::stream::StreamExt;
use tentacle::ble;
use uuid::Uuid;

/// Standard characteristics worth reading: a battery level and the strings that
/// say what the box thinks it is. Nothing vendor-specific gets read — a write
/// would be out of the question and an unknown read is not obviously harmless.
const KNOWN: &[(u16, &str, bool)] = &[
    (0x2A19, "battery level", false),
    (0x2A00, "device name", true),
    (0x2A24, "model number", true),
    (0x2A25, "serial number", true),
    (0x2A26, "firmware revision", true),
    (0x2A27, "hardware revision", true),
    (0x2A28, "software revision", true),
    (0x2A29, "manufacturer name", true),
];

#[tokio::main]
async fn main() -> Result<()> {
    let manager = Manager::new().await?;
    let central = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no bluetooth adapter"))?;

    let tentacle_service = uuid_from_u16(ble::SERVICE_UUID_16);
    let mut events = central.events().await?;
    central.start_scan(ScanFilter::default()).await?;

    // Collect the Tentacles by their service data, the same way the scanner
    // finds them — a name is whatever the owner typed.
    eprintln!("scanning for {tentacle_service} …");
    let mut found: BTreeMap<PeripheralId, String> = BTreeMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(12));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            event = events.next() => {
                let Some(CentralEvent::ServiceDataAdvertisement { id, service_data }) = event else {
                    continue;
                };
                if !service_data.contains_key(&tentacle_service) || found.contains_key(&id) {
                    continue;
                }
                let name = match central.peripheral(&id).await {
                    Ok(p) => p.properties().await.ok().flatten()
                        .and_then(|props| props.local_name.or(props.advertisement_name))
                        .unwrap_or_else(|| "<unnamed>".into()),
                    Err(_) => "<unnamed>".into(),
                };
                eprintln!("  found {name}");
                found.insert(id, name);
            }
        }
    }
    central.stop_scan().await?;

    if found.is_empty() {
        return Err(anyhow!("no Tentacle advertising {tentacle_service}"));
    }

    for (id, name) in &found {
        println!("\n=== {name}  [{id}]");
        if let Err(e) = probe(&central, id).await {
            println!("  failed: {e}");
        }
    }
    Ok(())
}

async fn probe(central: &btleplug::platform::Adapter, id: &PeripheralId) -> Result<()> {
    let peripheral = central.peripheral(id).await?;
    peripheral.connect().await?;
    let result = enumerate(&peripheral).await;
    let _ = peripheral.disconnect().await;
    result
}

async fn enumerate(peripheral: &btleplug::platform::Peripheral) -> Result<()> {
    peripheral.discover_services().await?;

    let characteristics = peripheral.characteristics();
    if characteristics.is_empty() {
        println!("  no characteristics discovered");
    }
    for ch in characteristics {
        let known = short(&ch.uuid).and_then(|s| KNOWN.iter().find(|(u, ..)| *u == s));
        let label = known.map_or(String::new(), |(_, name, _)| format!("  ({name})"));
        println!("  {} in {}  {:?}{label}", ch.uuid, ch.service_uuid, ch.properties);

        let Some((_, _, is_text)) = known else {
            continue;
        };
        match peripheral.read(&ch).await {
            Ok(bytes) if *is_text => println!("      = {:?}", String::from_utf8_lossy(&bytes)),
            Ok(bytes) => println!("      = {bytes:02x?}  ({:?} decimal)", bytes.first()),
            Err(e) => println!("      read failed: {e}"),
        }
    }
    Ok(())
}

/// The 16-bit form of a Bluetooth SIG UUID, or `None` if it isn't one.
fn short(uuid: &Uuid) -> Option<u16> {
    let bytes = uuid.as_bytes();
    (uuid_from_u16(u16::from_be_bytes([bytes[2], bytes[3]])) == *uuid)
        .then(|| u16::from_be_bytes([bytes[2], bytes[3]]))
}
