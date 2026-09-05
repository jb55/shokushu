# shokushu

Reads timecode off a Tentacle Sync E, two ways: `shokushu` decodes SMPTE LTC
from an audio input, and `shokushu-ble` reads it out of the device's Bluetooth
advertisements without pairing. `shokushu-rec` puts the two halves together and
records audio to a file stamped with the timecode a box is broadcasting.

The Bluetooth protocol is undocumented by the vendor; what's known about it is
written up in [PROTOCOL.md](PROTOCOL.md).

*shokushu* (触手) is Japanese for tentacle. This is an unofficial, unaffiliated
project: Tentacle Sync GmbH neither endorses nor supports it, and the hardware
is named here only to say what the thing reads.

```
$ shokushu-ble
adapter state: PoweredOn — scanning until interrupted
  11:12:00:16.5     25 fps   Ricki     2026-09-04   -43 dBm   100%
  11:11:44:00.3     25 fps   Liliana   2026-09-04   -51 dBm    96% +
```

```
$ shokushu
listening on External Microphone (48000 Hz, 1 ch, f32), channel 0
  01:23:45:12   29.97 fps   ub 00000000    -12.4 dBFS
```

```
$ shokushu --list-devices          # what inputs exist, and their ids
$ shokushu --device external       # pick one by id or by part of its name
$ shokushu --channel 1             # decode the right channel of a stereo input
$ shokushu --json                  # one JSON object per frame, for scripting
```

## shokushu-ble

The Sync E broadcasts its running timecode continuously, to anyone listening. No
pairing, no connection, no cable — which makes this the easy route on a Mac, and
the only one that works with the stock cable.

```
$ shokushu-ble                      # live timecode from any Tentacle in range
$ shokushu-ble --name ricki         # pick a device by name
$ shokushu-ble --json               # one JSON object per advert, uninterpolated
$ shokushu-ble --raw                # dump advertisements, marking changed bytes
```

Every Tentacle in range gets a line of its own, in the order they first turned
up, and each free-runs its own clock. Two boxes needn't be showing the same
timecode — or even running at the same frame rate — and each still ticks
correctly rather than the two of them fighting over one line.

The display free-runs between timecode advertisements. They arrive only one or
two times a second, so waiting for them meant the timecode jumped a dozen
frames at a time; instead each one anchors a local clock and the display is
redrawn at the frame rate, ticking the way a timecode display should. Anchoring
is smoothed, since snapping to every packet would let the display tick
backwards, and it never does. If nothing arrives for five seconds it stops
rather than inventing frames, and says so:

```
  10:03:40:16.8     25 fps   Ricki     2026-09-04   -46 dBm   100%   no signal for 6.1s
```

The percentage at the end of the line is the battery, and a `+` after it means
the device is on a charger.

Worth being clear about: interpolating makes the display *smooth*, not more
*accurate*. It adds no information the advertisements didn't carry. `--json` is
left alone for that reason — it emits the readings that actually arrived, and
nothing interpolated.


## shokushu-rec

Records an audio input to a Broadcast Wave file stamped with the timecode a
Tentacle is broadcasting. No cable between the box and the computer, and nothing
typed in afterwards — the file lands on somebody else's timeline in the right
place.

```
$ shokushu-rec --name ricki --device umc     # pick a box and an input
$ shokushu-rec --seconds 300                 # stop after five minutes
$ shokushu-rec --output take-1.wav           # otherwise named from the timecode
$ shokushu-rec --list-devices                # what inputs exist, and their ids
```

```
$ shokushu-rec --name ricki --device umc
adapter state: PoweredOn — recording from UMC202HD 192k (48000 Hz, 2 ch)
locked onto Ricki at 20:29:46:16 24 fps
first sample at 20:29:46:18 24 fps — writing ricki_2026-09-04_20-29-46-18.wav
● 00:00:29.9   20:30:16:13   24 fps   Ricki    -67.0 dBFS
wrote ricki_2026-09-04_20-29-46-18.wav — 29.995 s, 1439744 frames at 48000 Hz, 2 ch, 24-bit
  first sample at 20:29:46:18 24 fps, 3541765357 samples since midnight, 2026-09-04
  the input's clock ran -178 ppm against Ricki's over 30 s (±67 ppm)
```

## Using it as a library

The binaries are thin. Everything they do is in the crate, and `ble::Scanner`
is the way in — it owns the adapter, keeps a `Device` per box in range, and
anchors each one's clock as advertisements land.

There are two ways to read it, and both are wanted. Events tell you what
arrived:

```rust
use shokushu::ble::{Event, Scanner};

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
refresh rate wants this one — it's what `shokushu-ble` draws from:

```rust
for device in scan.devices() {
    match device.reading(Instant::now()) {
        Some(Reading::Running(tc)) => println!("{} {tc}", device.name().unwrap_or("?")),
        Some(Reading::Lost { last, since }) => println!("{last} — quiet for {since:?}"),
        None => {}  // not a Tentacle, or not one that has spoken up yet
    }
}
```

`cargo run --features scan --example scan` runs both side by side.

When nothing decodes, `scan.diagnosis()` says which of the failures it is —
nothing in range, something in range whose payload no longer decodes, or a scan
delivering nothing at all. It carries the counts and no wording, because the
sentence that suits a terminal names flags a GUI hasn't got; `shokushu-ble`
writes its own.

### The advertisement format

Documented in full in [PROTOCOL.md](PROTOCOL.md), with the evidence behind each
claim and what's still unknown — there's a formatted copy at
[docs/protocol.html](docs/protocol.html). None of it comes from a published
spec; `--raw` is how it was worked out. The short version:


### Features

The decoders have no dependencies and are always there: `ble::parse` for an
advertisement's bytes, `ltc::LtcDecoder` for audio samples, `freerun` to turn
either into a clock. None of them do I/O, so none of them can fail. Getting
hold of the bytes is what costs something, and that is what the features gate —
`scan` for Bluetooth (`btleplug` and a tokio runtime), `audio` for the LTC
binary's input (`cpal`), `cli` for the binaries.

None are on by default, so `cargo add shokushu` is the decoders alone — no
transport, no dependencies at all:

```toml
shokushu = "0.1"
```

Reading Bluetooth, without compiling an audio stack:

```toml
shokushu = { version = "0.1", features = ["scan"] }
```

The binaries in this repo declare the features they need, so running one from a
checkout names them:

```
cargo run --features scan,cli --bin shokushu-ble
cargo run --features audio,cli --bin shokushu
cargo run --features scan,audio,cli --bin shokushu-rec
```

`shokushu-rec` is the only one that needs both transports, which is what it is
for.

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

## License

MIT — see [LICENSE](LICENSE).
