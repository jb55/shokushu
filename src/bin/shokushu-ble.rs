//! Reads timecode off a Tentacle Sync E over Bluetooth LE, without pairing —
//! the device broadcasts it in its advertisements.
//!
//! Everything that talks to the adapter now lives in [`shokushu::ble::scan`];
//! what's left here is the display. Advertisements arrive only once or twice a
//! second, so the live display doesn't wait for them: each device keeps a local
//! clock ([`shokushu::freerun`]) that this samples at [`TICK`], which is what
//! makes the timecode tick smoothly instead of jumping. `--json` is left alone —
//! it emits the readings that actually arrived, and nothing interpolated.
//!
//! Every Tentacle in range gets a line of its own, since each keeps its own
//! clock: two boxes needn't be showing the same timecode, or even running at the
//! same frame rate.
//!
//! When nothing decodes there is nothing to draw, and a blank screen is the one
//! thing this must never be: `0xFDAC` service data whose payload has changed
//! looks exactly like an empty room. So the display says what it is taking in
//! instead. Which failure it is comes from [`shokushu::ble::diagnostics`]; the
//! wording is [`describe`], and stays here because it names command-line flags
//! that only exist here.
//!
//! `--drift` adds a column for how far each box's clock runs from this
//! computer's, which the free-running clock has to measure anyway in order to
//! extrapolate. It's off by default, and it goes on earning that. Watched for
//! seven minutes against two boxes, the figure used to wander over -45 to
//! +48 ppm and never settle, because the ten-second baseline it is measured
//! over was short against the jitter on the anchors either end of it. Filtering
//! those anchors — the clock now takes the least delayed reading of each window
//! rather than every reading, see [`Drift`] — cut that by about three, to a
//! spread of 5 to 6 ppm over a range of roughly ±23, measured over 1800 s
//! against a rate that came out at +7.7 ppm across the whole capture. Better, and still
//! not a number to quote off one look. Reading a single value as a property of
//! the box in front of you is the mistake this column invites, so
//! [`drift_column`] marks the two states where it is especially not one, and the
//! flag stays opt-in.
//!
//! Beside the ppm the column says how long that rate takes to add up to a
//! frame, which is the unit the question tends to get asked in: a rate is hard
//! to have a feel for and "one frame per two hours" isn't. It is the same
//! figure said differently and not a second one, so the marks in front of it
//! govern both — and being a reciprocal it is the more excitable of the two,
//! swinging from minutes to past a week and through "no slip at all" while the
//! estimate underneath it wanders across zero. [`slip_time`] is coarse for that
//! reason, and stops naming a number past [`SLIP_HORIZON`].
//!
//! Note also what it compares: two free-running oscillators, this host's
//! included, neither of them a reference. It says the two disagree, never which
//! of them is right.
//!
//! `--raw` turns this back into the reconnaissance tool it started as, dumping
//! advertisement payloads and marking which bytes changed. That's how the
//! layout in [`shokushu::ble`] was worked out, and it's the way to work out
//! anything still unknown — how a 29.97 drop-frame device differs, say.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use btleplug::platform::PeripheralId;
use clap::Parser;
use shokushu::ble::diagnostics::Diagnosis;
use shokushu::ble::{self, Advertisement, Date, Event, Scanner};
use shokushu::freerun::{Drift, Reading};
use shokushu::{Rate, Timecode};
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

    /// Add a column for how far each device's clock runs from this computer's,
    /// in ppm. Takes 10s to measure; `~` marks an estimate still converging and
    /// `!` one the model has stopped believing.
    #[arg(long)]
    drift: bool,
}

/// How often to redraw the live display. Comfortably above any frame rate a
/// Tentacle broadcasts, so each frame appears within a tick of when it starts,
/// and far too cheap to be worth tuning.
const TICK: Duration = Duration::from_millis(20);

/// How long a device that's gone quiet keeps its line.
///
/// Past [`shokushu::freerun::HOLDOVER`] a line freezes on the last reading that
/// arrived and counts up, which is worth seeing: reception is bursty and usually
/// comes back. A box switched off ten minutes ago isn't coming back and
/// shouldn't still be holding a line, so the line goes once it has been silent
/// this long. The device stays in the scan, so if it does return it reappears
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

/// What to call a device that never answered a properties lookup.
const UNNAMED: &str = "<unnamed>";

/// What the event loop should do next.
///
/// The scanner is borrowed by exactly one arm of the `select!`, and the borrow
/// has to be over before the loop body can have it back — so the arms hand back
/// a decision rather than acting on one.
enum Step {
    Stop,
    Draw,
    Took(Event),
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();

    let mut builder = Scanner::builder();
    if let Some(name) = &opt.name {
        builder = builder.name(name);
    }
    let mut scan = builder.start().await?;

    eprintln!(
        "adapter state: {:?} — scanning{}{}",
        scan.adapter_state().await?,
        match opt.seconds {
            0 => " until interrupted".to_string(),
            n => format!(" for {n}s"),
        },
        match &opt.name {
            Some(n) => format!(", filtering on {n:?}"),
            None => String::new(),
        }
    );

    let start = Instant::now();
    let deadline = async {
        match opt.seconds {
            0 => std::future::pending::<()>().await,
            n => tokio::time::sleep(Duration::from_secs(n)).await,
        }
    };
    tokio::pin!(deadline);

    // How many lines the last redraw left on screen, which is what the next one
    // has to move the cursor back up by.
    let mut drawn = 0usize;
    let mut notice = Notice::new();
    let mut raw: HashMap<PeripheralId, Payloads> = HashMap::new();
    let interpolating = !opt.raw && !opt.json;
    let mut ticker = tokio::time::interval(TICK);
    // Redraws are only worth doing on an even cadence. Catching up on ticks
    // missed while the event loop was busy would bunch several together, all
    // showing the same time.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let step = tokio::select! {
            _ = &mut deadline => Step::Stop,
            _ = ticker.tick(), if interpolating => Step::Draw,
            event = scan.next() => match event {
                Some(event) => Step::Took(event),
                None => Step::Stop,
            },
        };

        match step {
            Step::Stop => break,
            Step::Draw => render(
                &mut scan,
                &mut drawn,
                Instant::now(),
                &mut notice,
                opt.name.as_deref(),
                start.elapsed(),
                opt.drift,
            ),
            Step::Took(event) if opt.raw => {
                report_raw(&opt, &mut raw, &scan, start.elapsed().as_secs_f64(), event)
            }
            Step::Took(event) if opt.json => report_json(&scan, start, &event),
            // Nothing to do with it here: the scanner has already anchored the
            // clock this event carried, and `render` draws on its own schedule.
            Step::Took(_) => {}
        }
    }

    scan.stop().await?;
    // Every drawn line ends in a newline, so the cursor is already sitting on a
    // fresh one below the display; there is nothing to add to leave it there.
    // A diagnostic is the exception — it's deliberately left un-terminated so
    // it can be rewritten in place, so close it off rather than let the shell
    // prompt land on top of the last thing we said.
    notice.finish();
    Ok(())
}

/// One JSON object per reading that actually arrived, uninterpolated.
fn report_json(scan: &Scanner, start: Instant, event: &Event) {
    let Event::Timecode {
        id,
        timecode: tc,
        at,
    } = event
    else {
        return;
    };
    let Some(device) = scan.device(id) else {
        return;
    };
    // The drift trio goes on the end and every field before it is untouched,
    // so anything already reading this keeps working. Unlike the display it is
    // not behind --drift: a JSON stream is what a few minutes of this gets
    // analysed from, and a field that has to be asked for is one that turns out
    // to be missing from the capture you wanted it in.
    let drift = device.drift();
    // `host_micros` is this computer's clock at the moment the advertisement
    // came off the stream, in microseconds since the scan started. It's what
    // makes a capture analysable offline: every other field here is the
    // device's account of the time, and without a host stamp beside it there is
    // nothing to compare them against. It goes last for the same reason the
    // drift trio did — a reader that knows the older shape still works.
    //
    // Relative to the scan, not absolute, because it comes from an `Instant`:
    // a monotonic counter with no epoch to report. That's the right clock for
    // measuring an interval and the wrong one for saying when something
    // happened, and only the first is claimed here.
    println!(
        r#"{{"timecode":"{tc}","hours":{},"minutes":{},"seconds":{},"frames":{},"subframe_micros":{},"fps":{},"device":"{}","date":{},"rssi":{},"battery_percent":{},"charging":{},"drift_ppm":{},"drift_clamped":{},"drift_measurements":{},"frame_slip_seconds":{},"host_micros":{}}}"#,
        tc.hours,
        tc.minutes,
        tc.seconds,
        tc.frames,
        tc.subframe.as_micros(),
        tc.rate.fps,
        device.name().unwrap_or(UNNAMED),
        device.date().map_or("null".into(), |d| format!("\"{d}\"")),
        device.rssi().map_or("null".to_string(), |r| r.to_string()),
        device
            .battery()
            .map_or("null".to_string(), |b| b.battery_percent.to_string()),
        device
            .battery()
            .map_or("null".to_string(), |b| b.charging.to_string()),
        // Null rather than 0 until a baseline has been measured — see
        // `drift_column` for why a zero here would be a claim and not a
        // reading.
        drift.map_or("null".to_string(), |d| format!("{:.3}", d.ppm)),
        drift.map_or("null".to_string(), |d| d.clamped.to_string()),
        drift.map_or("null".to_string(), |d| d.measurements.to_string()),
        // The same rate said as a time, and null wherever there isn't one to
        // say — no drift measured yet, or a rate so near zero the answer
        // overflows. Both mean nothing has been resolved, which `null` says and
        // a very large number would not.
        drift
            .and_then(|d| d.frame_slip(tc.rate))
            .map_or("null".to_string(), |s| format!("{:.1}", s.as_secs_f64())),
        at.saturating_duration_since(start).as_micros(),
    );
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
    battery: Option<ble::Status>,
    /// `None` until this device has been heard from for long enough to
    /// measure. Whether the column is drawn at all is --drift's business, not
    /// this field's: the column has to hold its width from the first redraw,
    /// or every line shifts sideways ten seconds in.
    drift: Option<Drift>,
    note: String,
}

/// Draws a line per device from that device's free-running clock, so each ticks
/// between its own advertisements instead of only when one lands — and two boxes
/// in range don't fight over a single line.
fn render(
    scan: &mut Scanner,
    drawn: &mut usize,
    now: Instant,
    notice: &mut Notice,
    filter: Option<&str>,
    elapsed: Duration,
    show_drift: bool,
) {
    let mut rows = Vec::new();

    for device in scan.devices() {
        // No anchor yet means no clock to sample, which is also the filter that
        // keeps the display to Tentacles: a scan sees every peripheral the
        // adapter noticed, and most of them never send timecode.
        let Some(first) = device.first_timecode() else {
            continue;
        };
        let Some(reading) = device.reading(now) else {
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
            order: (first, short_id(device.id())),
            tc,
            name: device.name().unwrap_or(UNNAMED).to_string(),
            date: device.date(),
            rssi: device.rssi(),
            battery: device.battery(),
            drift: device.drift(),
            note,
        });
    }

    let lines = lay_out(rows, show_drift);

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
        let diagnosis = scan.diagnosis();
        notice.show(&diagnosis.key(), describe(&diagnosis, filter));
    }
}

/// One line saying what the scan is taking in, for when none of it decodes.
///
/// The classification is [`shokushu::ble::diagnostics`]'s; the words are this
/// program's, because they name this program's flags. Every case has to point at
/// something the reader can act on, since the alternative — which is what this
/// replaced — is a blank screen that means all of them at once.
fn describe(diagnosis: &Diagnosis, filter: Option<&str>) -> String {
    match diagnosis {
        Diagnosis::SilentScan => "no timecode: not one BLE advertisement of any kind has arrived, \
             so nothing is reaching this process — suspect the scan, not the Tentacles"
            .to_string(),

        // Nothing can fail a filter that isn't there, so `filter` is always set
        // in this arm — but say something true rather than unwrap on it.
        Diagnosis::FilteredOut { devices } => format!(
            "no timecode: {} in range, none named like {:?} — try again without --name",
            tally(*devices, "BLE device"),
            filter.unwrap_or_default(),
        ),

        Diagnosis::NothingAdvertising { devices } => format!(
            "no timecode: {} in range, none of which has advertised anything",
            tally(*devices, "BLE device")
        ),

        Diagnosis::Unreadable {
            name,
            payload,
            advertisers,
            payloads,
            unparsed,
        } => format!(
            "no timecode: {} advertising 0x{:04X}, but {unparsed} of {payloads} payloads did not \
             decode — {name} last sent {} (--raw -a dumps them all; see PROTOCOL.md)",
            tally(*advertisers, "device"),
            ble::SERVICE_UUID_16,
            hex(payload),
        ),

        Diagnosis::DatesOnly {
            advertisers,
            payloads,
        } => format!(
            "no timecode: {} advertising 0x{:04X} and all {payloads} payloads decoded, but none \
             has carried timecode yet — dates only so far",
            tally(*advertisers, "device"),
            ble::SERVICE_UUID_16,
        ),

        Diagnosis::NoTentacle { matched } => format!(
            "no timecode: {} in range, none advertising 0x{:04X} — no Tentacle here",
            tally(*matched, "BLE device"),
            ble::SERVICE_UUID_16,
        ),

        // `Diagnosis` is non-exhaustive, so a case added to the library reaches
        // here before it reaches this program. Say the key rather than nothing.
        other => format!("no timecode: {}", other.key()),
    }
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
    fn show(&mut self, key: &str, text: String) {
        if self.tty {
            if self.shown.as_deref() == Some(text.as_str()) {
                return;
            }
            eprint!("\r{text}\x1b[K");
            let _ = std::io::stderr().flush();
        } else {
            if self.key.as_deref() == Some(key) {
                return;
            }
            eprintln!("{text}");
        }
        self.shown = Some(text);
        self.key = Some(key.to_string());
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
/// The order has to come out the same on every redraw. Devices come back in
/// discovery order, which isn't the order their lines were established in, so
/// sorting on first-timecode is what keeps two boxes from swapping places.
fn lay_out(mut rows: Vec<Row>, show_drift: bool) -> Vec<String> {
    rows.sort_by(|a, b| a.order.cmp(&b.order));
    let name_width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);

    rows.iter()
        .map(|r| {
            format!(
                "  {}{:<3}   {:>3} fps   {:<name_width$}{}{}{}{}{}",
                r.tc,
                tenth(r.tc.subframe_fraction()),
                r.tc.rate.fps,
                r.name,
                r.date.map_or(String::new(), |d| format!("   {d}")),
                r.rssi.map_or(String::new(), |v| format!("   {v} dBm")),
                // Right-aligned, and the charging marker takes its two columns
                // whether or not it's showing, so neither a single-digit charge
                // nor a device on a cable shunts the note sideways.
                r.battery.map_or(String::new(), |b| {
                    format!(
                        "   {:>3}%{}",
                        b.battery_percent,
                        if b.charging { " +" } else { "  " }
                    )
                }),
                // Width held whether or not there's a figure yet, so the note
                // after it doesn't walk sideways when one arrives.
                if show_drift {
                    format!("   {:>26}", drift_column(r.drift, r.tc.rate))
                } else {
                    String::new()
                },
                r.note,
            )
        })
        .collect()
}

/// One device's clock drift, said in a way that can't be read as more than it
/// is.
///
/// Three things this has to refuse, and all three are the reason the column
/// isn't just a number:
///
/// - **Nothing measured yet.** For the first ten seconds a device is heard from
///   there is no measurement, only the nominal frame rate being assumed. That
///   would print as "0 ppm", which reads as a finding — a box in perfect
///   agreement with this computer — and is nothing of the kind. `— ppm` says
///   the column exists and has no answer for it.
/// - **Still converging**, marked `~`. The estimate starts at nominal and walks
///   a quarter of the way to each measurement, so it approaches the truth from
///   below over a minute or two. A `~+8 ppm` on its way to 40 is not a
///   measurement of 8.
/// - **Not believed**, marked `!`. The last measurement was past the few
///   hundred ppm the model will accept, so it was clipped. What's printed is
///   still the smoothed estimate — the mark is there to say the anchors it came
///   from were rejected, which is the difference between a device that runs at
///   the cap and a reading that ran off.
///
/// Rounded to a tenth of a ppm, which is 8.6 ms a day. That is far finer than
/// the figure is stable to — measured over seven minutes it moves by tens of
/// ppm — and the resolution is kept anyway, because watching it move is the
/// only thing on screen that says how little a single reading is worth.
fn drift_column(drift: Option<Drift>, rate: Rate) -> String {
    let Some(drift) = drift else {
        return "— ppm".to_string();
    };
    let mark = match (drift.clamped, drift.settling()) {
        (true, _) => "!",
        (false, true) => "~",
        (false, false) => "",
    };
    let slip = drift
        .frame_slip(rate)
        .map_or(NO_SLIP.to_string(), slip_time);
    format!("{mark}{:+.1} ppm  {slip}", drift.ppm)
}

/// What the slip half of the column says when there is no time to give: the
/// rate is zero, or so near it that a frame is further off than this is willing
/// to name.
const NO_SLIP: &str = "— /frame";

/// The horizon past which a slip time stops being a number and becomes "not
/// measurably drifting". It takes a rate of 0.005 ppm to reach it at 24 fps,
/// orders of magnitude finer than this estimate resolves, so everything past
/// here is the estimator on its way through zero and not a crystal that good.
const SLIP_HORIZON: Duration = Duration::from_secs(99 * 24 * 60 * 60);

/// A slip time in whatever unit keeps it readable.
///
/// Coarse on purpose, and coarser than the ppm beside it: the underlying figure
/// moves by tens of ppm between baselines, so a slip time quoted to the minute
/// would be inventing precision that the reciprocal has already stretched.
fn slip_time(d: Duration) -> String {
    if d > SLIP_HORIZON {
        return format!("> {} d/frame", SLIP_HORIZON.as_secs() / 86_400);
    }
    let s = d.as_secs_f64();
    match s {
        _ if s < 90.0 * 60.0 => format!("{:.0} min/frame", s / 60.0),
        _ if s < 48.0 * 3600.0 => format!("{:.1} h/frame", s / 3600.0),
        _ => format!("{:.1} d/frame", s / 86_400.0),
    }
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
/// of the frame it names (see [`shokushu::ble`]), so clamp rather than widen.
fn tenth(fraction: f64) -> String {
    format!(".{}", (fraction.clamp(0.0, 0.999) * 10.0) as u8)
}

/// The last payload seen under each key, so a dump can mark what changed.
///
/// Kept here rather than on a `Device`: the scanner's business is the Tentacle's
/// own records, and this remembers every company and every service UUID in the
/// room, which is a debugging tool's appetite rather than a library's.
#[derive(Default)]
struct Payloads {
    manufacturer: HashMap<u16, Vec<u8>>,
    service: HashMap<Uuid, Vec<u8>>,
}

/// Dumps a payload, with a caret under every byte that changed since last time.
fn report_raw(
    opt: &Opt,
    raw: &mut HashMap<PeripheralId, Payloads>,
    scan: &Scanner,
    elapsed: f64,
    event: Event,
) {
    let id = event.id().clone();
    let label = format!(
        "{} [{}]",
        scan.device(&id).and_then(|d| d.name()).unwrap_or(UNNAMED),
        short_id(&id)
    );

    match event {
        Event::Discovered { .. } => {
            println!("[{elapsed:7.3}s] {label}  discovered");
        }
        Event::Advertised { data, .. } => {
            let seen = raw.entry(id).or_default();
            match data {
                Advertisement::Manufacturer(records) => {
                    for (company, bytes) in records {
                        let previous = seen.manufacturer.get(&company).cloned();
                        if opt.all || previous.as_deref() != Some(bytes.as_slice()) {
                            print_payload(
                                elapsed,
                                &label,
                                &format!("mfr 0x{company:04x}"),
                                previous.as_deref(),
                                &bytes,
                            );
                        }
                        seen.manufacturer.insert(company, bytes);
                    }
                }
                Advertisement::Service(records) => {
                    for (uuid, bytes) in records {
                        let previous = seen.service.get(&uuid).cloned();
                        if opt.all || previous.as_deref() != Some(bytes.as_slice()) {
                            print_payload(
                                elapsed,
                                &label,
                                &format!("svc {uuid}"),
                                previous.as_deref(),
                                &bytes,
                            );
                        }
                        seen.service.insert(uuid, bytes);
                    }
                }
                _ => {}
            }
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
    use shokushu::ble::diagnostics::{diagnose, survey, Counts};

    /// The rate both boxes to hand run at, so a slip time asserted here is one
    /// that could have come off the display.
    const RATE: Rate = Rate {
        fps: 24,
        drop_frame: false,
    };

    fn row(order: (Instant, &str), name: &str) -> Row {
        Row {
            order: (order.0, order.1.into()),
            tc: Timecode {
                subframe: Duration::from_micros(12_000),
                ..Timecode::new(9, 44, 22, 13, Rate::whole(25))
            },
            name: name.into(),
            date: None,
            rssi: Some(-46),
            battery: Some(ble::Status { battery_percent: 97, charging: false }),
            drift: None,
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
    fn lines_keep_their_order_however_they_are_handed_over() {
        let early = Instant::now();
        let late = early + Duration::from_secs(1);
        let ricki = || row((early, "aabbccdd"), "Ricki");
        let bob = || row((late, "00112233"), "Bob");

        let forwards = lay_out(vec![ricki(), bob()], false);
        let backwards = lay_out(vec![bob(), ricki()], false);

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

        assert_eq!(lay_out(vec![one(), two()], false), lay_out(vec![two(), one()], false));
    }

    #[test]
    fn the_battery_column_keeps_its_width_at_every_charge() {
        // 100% and 7% are three characters apart written plainly, and a charging
        // marker adds two more, either of which would shunt the "no signal" note
        // sideways between one device and the next.
        let at = Instant::now();
        let mut full = row((at, "00112233"), "Bob");
        full.battery = Some(ble::Status { battery_percent: 100, charging: false });
        let mut low = row((at + Duration::from_secs(1), "aabbccdd"), "Ricki");
        low.name = "Bob".into();
        low.battery = Some(ble::Status { battery_percent: 7, charging: true });

        let lines = lay_out(vec![full, low], false);
        assert!(lines[0].contains("100%"), "{:?}", lines[0]);
        assert!(lines[1].contains("  7%"), "{:?}", lines[1]);
        assert_eq!(lines[0].len(), lines[1].len());
    }

    #[test]
    fn a_charging_device_is_marked_and_an_unplugged_one_is_not() {
        let at = Instant::now();
        let mut plugged = row((at, "00112233"), "Bob");
        plugged.battery = Some(ble::Status { battery_percent: 98, charging: true });
        let mut unplugged = row((at + Duration::from_secs(1), "aabbccdd"), "Bob");
        unplugged.battery = Some(ble::Status { battery_percent: 98, charging: false });

        let lines = lay_out(vec![plugged, unplugged], false);
        assert!(lines[0].contains("98% +"), "{:?}", lines[0]);
        assert!(!lines[1].contains('+'), "{:?}", lines[1]);
    }

    #[test]
    fn a_device_that_has_not_sent_its_battery_yet_leaves_the_column_out() {
        // The manufacturer record arrives as its own event, so a device can be
        // showing timecode before any charge is known. Better a missing column
        // than a made-up number.
        let at = Instant::now();
        let mut row = row((at, "00112233"), "Bob");
        row.battery = None;
        assert!(!lay_out(vec![row], false)[0].contains('%'));
    }

    /// The last payload of a device whose wire format has moved.
    const BROKEN: &[u8] = &[0x22, 0x7d, 0x19, 0x0b, 0x25, 0x28, 0x15, 0x5f, 0xc6];

    /// The sentence [`describe`] would put on screen, from a census of the
    /// devices a scan has looked at.
    fn text_of(matched: &[Counts<'_>], devices: usize, filter: Option<&str>) -> String {
        let census = survey(devices, matched);
        describe(&diagnose(&census, filter.is_some()), filter)
    }

    /// A device the scan has noticed, with whatever it has sent so far.
    fn device(name: &str, payloads: u64, unparsed: u64) -> Counts<'_> {
        Counts {
            name: Some(name),
            payloads,
            unparsed,
            unparsed_sample: (unparsed > 0).then_some(BROKEN),
        }
    }

    #[test]
    fn a_scan_delivering_nothing_says_so() {
        // The one case where the tool itself is the suspect: not even a passing
        // phone or a pair of headphones has been seen.
        let said = text_of(&[], 0, None);
        assert!(said.contains("not one BLE advertisement"), "{said}");
        assert!(said.contains("suspect the scan"), "{said}");
    }

    #[test]
    fn a_filter_that_matches_nothing_blames_the_filter() {
        // 40 devices in range and none of them looked at, which is a --name
        // typo far more often than it's an absent device.
        let said = text_of(&[], 40, Some("rikki"));
        assert!(said.contains("40 BLE devices"), "{said}");
        assert!(said.contains("\"rikki\""), "{said}");
        assert!(said.contains("without --name"), "{said}");
    }

    #[test]
    fn a_room_with_no_tentacle_in_it_says_that() {
        let phone = device("someone's phone", 0, 0);
        let watch = device(UNNAMED, 0, 0);
        let said = text_of(&[phone, watch], 2, None);
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
        let said = text_of(&[ricki, liliana], 72, None);

        assert!(said.contains("2 devices advertising 0xFDAC"), "{said}");
        assert!(said.contains("311 of 311 payloads did not decode"), "{said}");
        // The bytes are the whole point: a changed wire format can't be worked
        // out from a count of failures.
        assert!(said.contains("22 7d 19 0b 25 28 15 5f c6"), "{said}");
        assert!(said.contains("--raw -a"), "{said}");
    }

    #[test]
    fn one_device_reads_as_singular() {
        let ricki = device("Ricki", 9, 9);
        let said = text_of(&[ricki], 1, None);
        assert!(said.contains("1 device advertising"), "{said}");
        assert!(!said.contains("1 devices"), "{said}");
    }

    #[test]
    fn dates_arriving_without_timecode_is_its_own_case() {
        // Parsing fine and still nothing to show. Worth distinguishing: it
        // means the timecode record specifically is the thing that moved.
        let ricki = device("Ricki", 4, 0);
        let said = text_of(&[ricki], 3, None);
        assert!(said.contains("all 4 payloads decoded"), "{said}");
        assert!(said.contains("none has carried timecode"), "{said}");
    }

    #[test]
    fn a_notice_only_writes_when_the_words_change() {
        // Redirected to a file there is no cursor to rewrite, so an unchanged
        // message must not be re-emitted — the tick is 20 ms and the quiet case
        // can last minutes.
        let mut notice = Notice { tty: false, shown: None, key: None };

        // A count climbing under an unchanged key must not re-emit: the device
        // count ticks up for as long as the adapter keeps noticing the room.
        notice.show("no-tentacle", "29 devices".to_string());
        assert_eq!(notice.shown.as_deref(), Some("29 devices"));
        notice.show("no-tentacle", "37 devices".to_string());
        assert_eq!(notice.shown.as_deref(), Some("29 devices"));

        // A different failure is a different line.
        notice.show("unparsed:7d/9", "bytes moved".to_string());
        assert_eq!(notice.shown.as_deref(), Some("bytes moved"));
        // Including the same failure with a different payload shape, since a
        // wire format that moves twice is worth saying twice.
        notice.show("unparsed:7e/9", "moved again".to_string());
        assert_eq!(notice.shown.as_deref(), Some("moved again"));

        // And it gives the line back when the display wants it.
        notice.clear();
        assert_eq!(notice.shown, None);
        assert_eq!(notice.key, None);
    }

    /// A settled, believed drift figure of `ppm`.
    fn measured(ppm: f64) -> Option<Drift> {
        Some(Drift { ppm, clamped: false, measurements: 20 })
    }

    #[test]
    fn the_drift_column_is_absent_unless_it_was_asked_for() {
        let at = Instant::now();
        let mut r = row((at, "00112233"), "Bob");
        r.drift = measured(12.4);
        assert!(!lay_out(vec![r], false)[0].contains("ppm"));
    }

    #[test]
    fn an_unmeasured_drift_is_not_reported_as_zero() {
        // The lie this column exists to avoid. For the first ten seconds the
        // clock is running at the nominal rate because nothing has measured it
        // yet, and printing that as "+0.0 ppm" would read as a device in
        // perfect agreement with this computer.
        assert_eq!(drift_column(None, RATE), "— ppm");
        assert!(!drift_column(None, RATE).contains('0'));
    }

    #[test]
    fn a_settled_measurement_is_a_signed_number_and_a_time() {
        // 12.4 ppm at 24 fps loses a 41.7 ms frame in 3360 s.
        assert_eq!(drift_column(measured(12.4), RATE), "+12.4 ppm  56 min/frame");
        assert_eq!(drift_column(measured(-3.0), RATE), "-3.0 ppm  3.9 h/frame");
    }

    #[test]
    fn a_slip_time_says_nothing_about_which_clock_is_ahead() {
        // The sign belongs to the ppm. How long they take to part is the same
        // either way, and a slip time that changed with the direction would be
        // claiming otherwise.
        let fast = drift_column(measured(9.0), RATE);
        let slow = drift_column(measured(-9.0), RATE);
        let tail = |c: &str| c.split_once("ppm  ").unwrap().1.to_string();
        assert_eq!(tail(&fast), tail(&slow));
        assert_ne!(fast, slow);
    }

    #[test]
    fn a_rate_through_zero_stops_naming_a_time_rather_than_naming_a_huge_one() {
        // The reciprocal's bad end. The estimate wanders across zero, and the
        // "4800 d/frame" that 0.0001 ppm works out to would read as a crystal
        // orders of magnitude better than this can resolve, rather than as an
        // estimator on its way through nothing.
        assert!(drift_column(measured(0.0), RATE).ends_with(NO_SLIP));
        assert!(drift_column(measured(0.0001), RATE).ends_with("> 99 d/frame"));
    }

    #[test]
    fn an_estimate_still_converging_is_marked() {
        // It approaches from below over a minute or two, so an early figure is
        // a number on its way somewhere rather than a reading.
        let early = Some(Drift { ppm: 8.0, clamped: false, measurements: 1 });
        assert_eq!(drift_column(early, RATE), "~+8.0 ppm  87 min/frame");
    }

    #[test]
    fn a_clamped_estimate_is_distinguishable_from_a_real_one_at_the_cap() {
        // The requirement that made this a column and not a number: a device
        // genuinely running at the model's limit and a measurement the model
        // threw out must not print the same.
        let honest = Drift { ppm: 500.0, clamped: false, measurements: 20 };
        let refused = Drift { clamped: true, ..honest };
        assert_ne!(
            drift_column(Some(honest), RATE),
            drift_column(Some(refused), RATE)
        );
        assert_eq!(drift_column(Some(refused), RATE), "!+500.0 ppm  1 min/frame");

        // And the refusal outranks the settling mark, since "these anchors were
        // rejected" is the more important of the two things to say.
        let both = Drift { clamped: true, measurements: 1, ..honest };
        assert!(drift_column(Some(both), RATE).starts_with('!'));
    }

    #[test]
    fn the_drift_column_holds_its_width_before_it_has_a_figure() {
        // Ten seconds into every run the first measurement lands. If the column
        // grew then, every line on screen would shift sideways at once.
        let at = Instant::now();
        let mut waiting = row((at, "00112233"), "Bob");
        waiting.note = "   x".into();
        let mut knows = Row { drift: measured(-123.4), ..row((at, "00112233"), "Bob") };
        knows.note = "   x".into();

        let width = |r: Row| lay_out(vec![r], true)[0].chars().count();
        assert_eq!(width(waiting), width(knows));
    }

    #[test]
    fn the_name_column_is_padded_to_the_widest_name() {
        let at = Instant::now();
        let lines = lay_out(vec![
            row((at, "00112233"), "Bob"),
            row((at + Duration::from_secs(1), "aabbccdd"), "Ricki"),
        ], false);

        // Identical but for the name, so equal length means what follows the
        // name lines up between the two.
        assert_eq!(lines[0].len(), lines[1].len());
    }
}
