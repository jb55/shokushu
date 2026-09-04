# tentacle

Reads timecode off a Tentacle Sync E, two ways: `tentacle` decodes SMPTE LTC
from an audio input, and `tentacle-ble` reads it out of the device's Bluetooth
advertisements without pairing.

The Bluetooth protocol is undocumented by the vendor; what's known about it is
written up in [PROTOCOL.md](PROTOCOL.md).

```
$ tentacle-ble
adapter state: PoweredOn — scanning until interrupted
  11:12:00:16.5     25 fps   Ricki     2026-09-04   -43 dBm   100%
  11:11:44:00.3     25 fps   Liliana   2026-09-04   -51 dBm    96% +
```

```
$ tentacle
listening on External Microphone (48000 Hz, 1 ch, f32), channel 0
  01:23:45:12   29.97 fps   ub 00000000    -12.4 dBFS
```

```
$ tentacle --list-devices          # what inputs exist, and their ids
$ tentacle --device external       # pick one by id or by part of its name
$ tentacle --channel 1             # decode the right channel of a stereo input
$ tentacle --json                  # one JSON object per frame, for scripting
```

## tentacle-ble

The Sync E broadcasts its running timecode continuously, to anyone listening. No
pairing, no connection, no cable — which makes this the easy route on a Mac, and
the only one that works with the stock cable.

```
$ tentacle-ble                      # live timecode from any Tentacle in range
$ tentacle-ble --name ricki         # pick a device by name
$ tentacle-ble --json               # one JSON object per advert, uninterpolated
$ tentacle-ble --raw                # dump advertisements, marking changed bytes
```

Every Tentacle in range gets a line of its own, in the order they first turned
up, and each free-runs its own clock. Two boxes needn't be showing the same
timecode — or even running at the same frame rate — and each still ticks
correctly rather than the two of them fighting over one line.

The display free-runs between advertisements. They arrive only one or two times
a second, so waiting for them meant the timecode jumped a dozen frames at a
time; instead each one anchors a local clock and the display is redrawn at the
frame rate, ticking the way a timecode display should. Anchoring is smoothed,
since snapping to every packet would let the display tick backwards, and it
never does. If nothing arrives for five seconds it stops rather than inventing
frames, and says so:

```
  10:03:40:16.8     25 fps   Ricki     2026-09-04   -46 dBm   100%   no signal for 6.1s
```

Reception is bursty enough that a gap is usually worth waiting out, so the line
stays, frozen on the last reading that arrived. After thirty seconds of silence
it goes: by then the box has been switched off rather than merely missed.

The percentage at the end of the line is the battery, and a `+` after it means
the device is on a charger. Both come from a manufacturer-data field the device
broadcasts alongside the timecode, so they appear a second or two after the line
itself — the two are separate advertisements. There is no Battery Service to read
over GATT; this is the only place a Sync E publishes its charge, and reading it
costs nothing, since it is in a broadcast that was being listened to anyway. How
that was established, and how far the scale is actually pinned down, is in
[PROTOCOL.md](PROTOCOL.md).

When there is nothing to show, it says why rather than showing nothing. An
empty screen used to mean any of three things — no Tentacle in range, a
Tentacle in range whose advertisement no longer decodes, or a scan delivering no
events at all — and looked identical in all three. After three seconds without
timecode it reports what it is actually taking in, on stderr, as a single line
that updates in place:

```
$ tentacle-ble
adapter state: PoweredOn — scanning until interrupted
no timecode: 2 devices advertising 0xFDAC, but 22 of 22 payloads did not decode — Liliana last sent 22 7d 19 0b 3b 13 00 93 bf (--raw -a dumps them all; see PROTOCOL.md)
```

That one is the failure that prompted this: a header byte changed and every
advertisement was being rejected. Other things it will tell you are that a
`--name` filter matched none of the devices in range, that devices are in range
but none of them advertise `0xFDAC`, or that nothing at all is arriving — which
points at the scan rather than the boxes. The bytes are included because a wire
format that has moved cannot be worked out from a count of failures.

It stays out of the way once timecode is flowing: the line is given up the
moment there is a reading to draw, `--json` emits no diagnostics at all, and
redirecting stderr to a file gets one line per thing that actually changed
rather than fifty a second.

Worth being clear about: interpolating makes the display *smooth*, not more
*accurate*. It adds no information the advertisements didn't carry. `--json` is
left alone for that reason — it emits the readings that actually arrived, and
nothing interpolated.

The accuracy comes from the sub-frame field instead: it's a microsecond counter,
which places a reading to about 0.6 ms where the frame number alone manages
13 ms. What can't be done from here is clocking anything — one or two readings a
second tells you what time it is, and for syncing to picture you still want LTC
over audio.

macOS will ask for Bluetooth permission the first time.

### The advertisement format

Documented in full in [PROTOCOL.md](PROTOCOL.md), with the evidence behind each
claim and what's still unknown — there's a formatted copy at
[docs/protocol.html](docs/protocol.html). None of it comes from a published
spec; `--raw` is how it was worked out. The short version:

```
22 7d | 19 0b 25 28 15 | 5f c6     fps=25, 11:37:40:21, 24518 µs into the frame
42 7d | 00 26 09 04 02 | a1 00     2026-09-04
```

Service UUID `0xFDAC`, nine bytes, a record type and a flags byte ahead of a
five-byte data field, with a big-endian microsecond counter in the trailer.

Note the layout is **fixed, not self-describing**. Byte 1 held `0x05` in every
early capture, which is exactly the width of the data field, and reading it as a
length looked safe for as long as nobody changed it. Syncing the boxes to the
Tentacle phone app changed it to `0x7d` while the packets stayed nine bytes, at
which point every advertisement was rejected and the scanner showed an empty
screen with no explanation. Don't derive the field from it.

Three things to know before extending this. **Discovery has to key on the service
UUID, not on a name** — the advertised name is whatever the owner called the
device, so a scanner looking for "Tentacle" finds nothing. **Timecode is plain
binary, not BCD**, which is easy to get backwards because most samples look like
valid BCD; the date record, inconsistently, *is* BCD. And **the frame rate
arrives as a whole number**, so 29.97 and 30 are indistinguishable over the air
and no drop-frame flag is broadcast at all. Only 25 fps has ever been observed.

### tentacle-probe

Everything above listens and never transmits. `tentacle-probe` is the exception,
and is kept separate for that reason: it connects to each Tentacle in range,
lists its GATT services and characteristics, and reads the standard Device
Information and battery ones.

```
$ tentacle-probe
=== Ricki  [7806a574-7711-abac-3737-c42b79c16804]
  00002a29-…  (manufacturer name)   = "Tentacle Sync GmbH"
  00002a27-…  (hardware revision)   = "1.2 SYNCE2"
  …
```

It exists to answer a question — is the charge level available anywhere other
than the advertisement? — and the answer is no: there is no Battery Service on
this device. Reach for it when a new firmware appears and that might have
changed, not as part of reading timecode. Connecting is not free; it can disturb
the advertising that the rest of this depends on, and it is per-device. The
vendor characteristics, one of which is writable, are listed but never touched.

## Using it as a library

The binaries are thin. Everything they do is in the crate, and `ble::Scanner`
is the way in — it owns the adapter, keeps a `Device` per box in range, and
anchors each one's clock as advertisements land.

There are two ways to read it, and both are wanted. Events tell you what
arrived:

```rust
use tentacle::ble::{Event, Scanner};

let mut scan = Scanner::builder().name("ricki").start().await?;
while let Some(event) = scan.next().await {
    match event {
        Event::Timecode { timecode, .. } => println!("{timecode}"),
        Event::Battery { status, .. } => println!("{}%", status.battery_percent),
        _ => {}
    }
}
```

The clocks tell you what time it is *now*, which is a different question.
Advertisements land once or twice a second, so anything drawing at its own
refresh rate wants this one — it's what `tentacle-ble` draws from:

```rust
for device in scan.devices() {
    match device.reading(Instant::now()) {
        Some(Reading::Running(tc)) => println!("{} {tc}", device.name().unwrap_or("?")),
        Some(Reading::Lost { last, since }) => println!("{last} — quiet for {since:?}"),
        None => {}  // not a Tentacle, or not one that has spoken up yet
    }
}
```

`cargo run --example scan` runs both side by side.

When nothing decodes, `scan.diagnosis()` says which of the failures it is —
nothing in range, something in range whose payload no longer decodes, or a scan
delivering nothing at all. It carries the counts and no wording, because the
sentence that suits a terminal names flags a GUI hasn't got; `tentacle-ble`
writes its own.

### Features

The decoders have no dependencies and are always there: `ble::parse` for an
advertisement's bytes, `ltc::LtcDecoder` for audio samples, `freerun` to turn
either into a clock. None of them do I/O, so none of them can fail. Getting
hold of the bytes is what costs something, and that is what the features gate —
`scan` for Bluetooth (`btleplug` and a tokio runtime), `audio` for the LTC
binary's input (`cpal`), `cli` for the binaries.

All three are on by default so `cargo run` works as it always has. Reading
Bluetooth without compiling an audio stack:

```toml
tentacle = { version = "0.1", default-features = false, features = ["scan"] }
```

Or the decoders alone, with no transport and no dependencies at all:

```toml
tentacle = { version = "0.1", default-features = false }
```

### Timecode

Both sources decode into one `Timecode`, carrying a `Rate`. Neither can fill a
`Rate` on its own, and they fail in opposite directions: the advertisement
carries a whole frame rate and no drop-frame flag, so 29.97 and 30 are
indistinguishable over the air, while LTC carries the drop-frame flag and no
rate at all — its rate is inferred from the bit period the decoder locked to.
So `drop_frame: false` on a Bluetooth reading means *unknown*, not *known not
to be drop-frame*.

Drop-frame arithmetic is deliberately not implemented: drop-frame skips frame
*numbers*, so `frame_position` would be wrong for it. It debug-asserts, and
`checked_frame_position` returns `None`, rather than quietly handing back a
number that's off by a couple of seconds a day.

## Wiring it to a Mac

The catch is the 3.5 mm jack. It auto-detects what's plugged in, and a
three-conductor **TRS** plug — which is what the Tentacle's standard cable ends
in — reads as headphones. macOS then offers an *output* on the jack and no input
at all, so there is nothing for this program to listen to. Check with:

```
$ system_profiler SPAudioDataType | grep -A2 'External'
```

If that says `External Headphones` and `--list-devices` shows no external input,
the signal isn't reaching the computer. Two ways around it:

- **A TRRS adapter.** The jack only exposes a microphone on the sleeve of a
  four-conductor CTIA plug. A TRRS headset splitter (one TRRS male out to
  separate mic and headphone TRS females) works: plug the Tentacle into the
  microphone side. macOS then shows an `External Microphone` device.
- **A USB audio interface.** Any interface with a line or mic input, which also
  avoids the level mismatch below.

The Tentacle's output is hotter than a computer mic input expects. Clipping does
no harm here — LTC is a square wave and the decoder only looks at where it
crosses zero — but if the input distorts badly, pad it or turn the Tentacle's
output level down in the Tentacle app.

macOS will also ask for microphone permission the first time; without it the
stream opens but delivers silence.

## How LTC decoding works

LTC packs 80 bits into every video frame, biphase-mark encoded: each bit cell
opens with a transition, and a `1` adds a second one in the middle. So a `0` is
one long gap between transitions and a `1` is two short ones. That makes the
code self-clocking, readable at any speed, and indifferent to which way round
the cable is wired.

`src/ltc.rs` does the decoding in three stages, all streaming and allocation-free:

1. **Transition detection.** A one-pole DC blocker removes the offset, a decaying
   peak envelope sets a hysteresis threshold at 25% of the signal, and crossings
   of that threshold are timed in samples.
2. **Biphase demodulation.** An interval longer than ¾ of the running bit-period
   estimate is a `0`; two shorter ones make a `1`. Each decoded cell nudges the
   period estimate, so the decoder locks onto 24/25/29.97/30 fps on its own
   rather than being told the rate.
3. **Frame assembly.** Bits shift into an 80-bit register. The last 16 bits of
   every frame are the sync word `0x3FFD` — twelve consecutive ones, which the
   payload can't produce — so spotting it exactly 80 bits after the previous one
   both confirms alignment and delimits the frame. BCD fields out of range are
   rejected, which catches a bit slip that happened to land on a sync pattern.

The frame rate is reported from the locked bit period. 23.976 and 29.97 sit
within 0.1% of 24 and 30, closer than that estimate resolves; the drop-frame flag
is the only reliable way to tell 29.97 apart, and there's no way at all to
distinguish 23.976 from 24.

Reverse playback (LTC read tail-first) isn't decoded — a free-running generator
like the Tentacle never produces it.

## Tests

`cargo test` runs the LTC decoder against synthesized audio from a biphase-mark
encoder in the test module, covering several frame rates and sample rates,
minute rollover, drop-frame and user bits, an inverted signal, recovery after a
dropout, arbitrary buffer boundaries, and rejection of silence, tones and noise.

The BLE parser is tested against payloads captured off real hardware, including
the minute rollover that proves the fields are binary, and a spread of malformed
packets it has to reject. The free-running clock is tested against synthesized
anchors: that it walks every frame between two adverts, never goes backwards
under jitter far worse than reception really is, stays inside a frame of a
device whose crystal drifts, reports signal loss instead of extrapolating
through it, and snaps rather than slews when the timecode is changed on the
device.
