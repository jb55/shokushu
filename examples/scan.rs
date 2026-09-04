//! The two ways to read a scan, side by side.
//!
//! Events tell you what arrived; the clocks tell you what time it is now. This
//! prints a line per advertisement and, once a second, what every device's
//! clock says at that instant — which is a different number, because the clock
//! has been running since the last advertisement landed.
//!
//! ```console
//! $ cargo run --example scan
//! ```

use std::time::{Duration, Instant};

use shokushu::ble::{Event, Scanner};
use shokushu::freerun::Reading;

#[tokio::main]
async fn main() -> shokushu::Result<()> {
    let mut scan = Scanner::start().await?;
    let mut next_poll = Instant::now() + Duration::from_secs(1);

    loop {
        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + Duration::from_secs(1);
            for device in scan.devices() {
                let name = device.name().unwrap_or("<unnamed>").to_string();
                match device.reading(now) {
                    Some(Reading::Running(tc)) => println!("  clock  {name}  {tc} {}", tc.rate),
                    Some(Reading::Lost { last, since }) => {
                        println!("  clock  {name}  {last} — quiet for {:.1}s", since.as_secs_f64())
                    }
                    // Not a Tentacle, or not one that has spoken up yet.
                    None => {}
                }
            }
        }

        let Some(event) = scan.next().await else { break };
        match event {
            Event::Timecode { timecode, .. } => println!("advert   {timecode}"),
            Event::Battery { status, .. } => println!("battery  {}%", status.battery_percent),
            Event::Lost { last, .. } => println!("lost     {last}"),
            _ => {}
        }
    }

    scan.stop().await
}
