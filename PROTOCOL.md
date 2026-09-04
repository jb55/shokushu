# Tentacle Sync E — protocol notes

How to read timecode off a Tentacle Sync E Mk2, three ways. **Nothing here comes
from a published specification.** It is what the bytes did when watched against a
device whose timecode and date were known.

A formatted version lives at [`docs/protocol.html`](docs/protocol.html), and is
published at <https://claude.ai/code/artifact/c47322ac-9c31-49f9-8a9e-0bac100f026b>.

Every claim below is marked:

| Mark | Meaning |
|---|---|
| **[measured]** | directly observed |
| **[inferred]** | consistent with the data, not proven |
| **[unknown]** | no idea |

Corpus: 338 unique BLE payloads over roughly 275 s, plus a fourth capture for
reception timing and an independent fifth by a second implementation — all from
**one** device at **one** frame rate (25 fps), on 2026-09-04. That last part is
the main limitation of everything here.

## Three transports

| Transport | Rate | Precision | Availability |
|---|---|---|---|
| BLE advertisements | ~1.4–1.8/s | <1 ms per packet | Always on, no pairing |
| LTC on audio out | every frame | sample-accurate | Needs a real audio input |
| USB-C | — | — | Vendor-specific, undocumented |

The trade is rate against precision. A BLE packet carries a microsecond stamp, so
any single reading is excellent, but they arrive under twice a second — nothing
can be clocked from them without interpolation. LTC arrives continuously and is
what you want for syncing to picture.

## BLE advertisements

The device broadcasts continuously to anyone listening. No pairing, no
connection, no GATT read — the timecode is in the advertisement itself.

| | |
|---|---|
| Service UUID | `0000fdac-0000-1000-8000-00805f9b34fb` |
| Short form | `0xFDAC` |
| Service data | 9 bytes, always |
| Local name | user-set in the Tentacle app |

**Discovery must key on the service UUID, not on a name.** The advertised name is
whatever the owner called the device — ours was "Ricki". A scanner looking for
"Tentacle" finds nothing. [measured]

### Reception characteristics

Two things about how packets actually arrive, both of which will bite an
implementer who assumes a steady stream.

**Every reading arrives twice.** [measured] Of 75 distinct readings in a 45 s
capture, 72 were delivered as byte-identical pairs 0–1 ms apart; only 3 arrived
singly. The raw advertisement rate was 3.41/s and the fresh-reading rate 1.74/s.

This is not a Tentacle quirk — it is ordinary BLE. One advertising event
retransmits the same PDU on each of the three primary advertising channels, and a
scanner that catches more than one copy reports it more than once. **De-duplicate
on payload equality**, or your apparent rate is double the real one.

**Reception is bursty.** [measured] Gaps between *fresh* readings, three captures
with the device on a desk at −46 dBm:

| Capture | Fresh/s | p50 | p90 | max | Gaps >1 s |
|---|---|---|---|---|---|
| A (n=71) | 1.80 | 320 ms | 1481 ms | 2206 ms | 14% |
| B (n=113) | 1.63 | 519 ms | 1169 ms | 1887 ms | 21% |
| C (n=55, independent) | 1.38 | 320 ms | 1480 ms | 2110 ms | ~25% |

Half the gaps are around a third of a second, but a fifth to a quarter exceed a
full second and the worst run past two. **A signal-loss timeout under about 3 s
will fire during normal reception**; 5 s is a safer holdover. Good line-of-sight
and a strong RSSI do not fix this — the figures above are near-ideal conditions.

### Packet anatomy

Every payload is nine bytes with the same shape. [measured]

```
   22   05   19 09 23 3b 14   58 62
   ~~   ~~   ~~~~~~~~~~~~~~   ~~~~~
   |    |    |                `-- trailer, big-endian u16
   |    |    `-- data field, five bytes, meaning set by type
   |    `-- length, always 0x05
   `-- record type
```

| Byte 0 | Record | Share of traffic |
|---|---|---|
| `0x22` | Timecode | 337 of 348 |
| `0x42` | Date | 11 of 348 |

Byte 1 was `0x05` in every packet observed, and every packet was nine bytes.
Reading it as a length is self-consistent but untested — no packet of another
size ever turned up. [inferred]

### Timecode record — `0x22`

| Byte | Field | Encoding | Observed | |
|---|---|---|---|---|
| 2 | Frame rate | binary, whole fps | 25 only | [inferred] |
| 3 | Hours | binary | 9 only | [inferred] |
| 4 | Minutes | binary | 0–59 | [measured] |
| 5 | Seconds | binary | 0–59, all 60 values | [measured] |
| 6 | Frames | binary | 0–24, all 25 values | [measured] |
| 7–8 | Microseconds into frame | big-endian u16 | 3685–43581 | [measured] |

**Timecode is plain binary, not BCD.** [measured] This is the trap. Samples like
`09 20 39` read convincingly as BCD 09:20:39 — every nibble is a valid digit. It
is wrong. The seconds byte was observed taking all 60 values from `0x00` to
`0x3b`, and a captured minute rollover settles it:

```
22 05 19 09 23 3b 14 58 62     minute 0x23, second 0x3b = 59
22 05 19 09 24 00 03 58 ca     minute 0x24, second 0x00
```

`0x3b` is 59 in binary and nonsense in BCD. So that first sample was really
**09:32:57**. The date record, just to be difficult, *is* BCD.

**Frames run 0 to fps−1.** [measured] All 25 values `0x00`–`0x18` appeared and
none above, against a frame-rate byte of 25. That agreement is also the main
support for reading byte 2 as the frame rate at all.

### The microsecond field (bytes 7–8)

The most useful part of the packet and the hardest to pin down.

**It is not a CRC.** [measured] Two independent results. No standard CRC-16 —
across the usual polynomials, initial values, reflections and output masks —
reproduces the trailer from the preceding seven bytes. And solving over GF(2)
rules out the whole family at once: if the trailer were any linear function of
the payload, differences between payloads would map consistently onto differences
between trailers. Gaussian elimination produced **250 contradictions**. That
eliminates every CRC, parity and XOR-style checksum, whatever its parameters.

**It is microseconds, at roughly 1 MHz.** [measured] Timing identifies it.
Compare each packet's timecode against the host clock when it arrived, and see
which interpretation tracks best. Median absolute residual across three
independent captures — A and B trained and validated separately, C collected
afterwards by a second implementation that had not seen the others (one frame at
25 fps = 40 ms):

| Interpretation | A (n=71) | B (n=113) | C (n=112) |
|---|---|---|---|
| Ignore the trailer | 8.0 ms | 9.0 ms | 13.5 ms |
| Fraction of a frame, ÷65536 | 3.1 ms | 3.5 ms | 4.97 ms |
| **Microseconds, ÷40000** | **0.61 ms** | **0.73 ms** | **0.61 ms** |

Ignoring the trailer leaves 8–13 ms of error, on the order of a quarter frame —
the signature of pure quantisation. Reading it as microseconds collapses that by
more than tenfold, to well under a millisecond, every time.

Two further checks agree. Free-fitting the scale with no assumptions lands on
39,728, 39,376 and 40,500 across the three captures — a ~1 MHz tick divided into
frames. And the observed value range is the right width for that and nowhere near
65536: span **39,896** in A and B combined, **39,251** in C, against 40,000 µs in
a frame at 25 fps.

**A constant few-millisecond bias.** [unknown] Values range 3685–43581 in
captures A and B, and 4056–43307 in C — not 0–39999. The
window is the right width but offset by about 3.7 ms, so this is not literally
"microseconds since the frame boundary". The origin is unknown; a transmit-path
constant is a plausible guess and nothing more. It does not matter for
interpolation — a constant offset cancels the moment you use a packet as an
anchor and extrapolate. It does matter if you want absolute phase.

**Scaling by frame rate is an inference.** [inferred] Only a 25 fps device was
observed, so `1_000_000 / fps` microseconds per frame is the natural reading of a
1 MHz counter rather than something demonstrated at a second rate. Scale by fps
rather than hardcoding 40000.

### Date record — `0x42`

Roughly one packet in thirty, about every 11 seconds. Carries the date the device
is set to — the same date it writes into the LTC user bits.

```
   42   05   00 26 09 04 02   a1 00
             |  |  |  |  |
             |  |  |  |  `-- always 0x02  [unknown]
             |  |  |  `-- day,   BCD
             |  |  `-- month, BCD
             |  `-- year,  BCD (0x26 -> 2026)
             `-- always 0x00  [unknown]
```

Unlike the timecode record this one is BCD: `0x26 0x09 0x04` reads as 26-09-04 and
the device's date was 4 September 2026. Read as binary it would be 38-09-04, which
is not a date. [measured]

**The trailer here is a constant, not a counter.** [measured] All 11 date records
were byte-for-byte identical, trailer included. Whatever bytes 7–8 mean in a
timecode record, they mean something else here — or nothing.

### Manufacturer data

Advertised alongside the service data, and never changed across any capture.

```
company 0x043f    02 00 64 01 13
```

`0x64` is 100 and the battery was full throughout, so "battery percent" is an
obvious guess and an untested one. [unknown]

## LTC over audio

The audio output is ordinary SMPTE 12M linear timecode — a documented standard,
unlike everything above.

80 bits per video frame, biphase-mark encoded: every bit cell opens with a
transition, and a `1` adds a second one in the middle. A `0` is one long interval
between transitions, a `1` is two short ones. Self-clocking, readable at any
speed, and indifferent to polarity — a swapped tip and ring changes nothing.

| Bits | Field | Bits | Field |
|---|---|---|---|
| 0–3 | Frame units | 32–35 | Minute units |
| 8–9 | Frame tens | 40–42 | Minute tens |
| 10 | Drop-frame flag | 48–51 | Hour units |
| 11 | Colour-frame flag | 56–57 | Hour tens |
| 16–19 | Second units | 4, 12, 20 … 60 | User bits, 8 nibbles |
| 24–26 | Second tens | 64–79 | Sync word `0x3FFD` |

Fields are little-endian BCD. The sync word makes a free-running bit stream
parseable: twelve consecutive ones cannot occur in the payload, so finding
`0x3FFD` exactly 80 bits after the last one both confirms alignment and delimits
the frame.

### Getting it into a Mac

The cable in the box is TRS-to-TRS, correct for a camera, where the 3.5 mm socket
is a dedicated input. A Mac's jack is a *headset* jack with a different pinout:

| | Camera mic input (TRS) | Mac headset jack (CTIA TRRS) |
|---|---|---|
| Tip | audio **in** (L) | headphone **out** (L) |
| Ring 1 | audio **in** (R) | headphone **out** (R) |
| Ring 2 | — | ground |
| Sleeve | ground | microphone **in** |

Insert a three-conductor plug and its long sleeve bridges the ground and mic
contacts — which is exactly how macOS concludes "headphones, no mic". The signal
has nowhere to go. Check with `system_profiler SPAudioDataType`: if it reports
`External Headphones` and no external input appears, you need a CTIA TRRS adapter
or a USB audio interface.

## USB

The USB-C port charges the device and speaks a vendor protocol to the Tentacle
app. It presents no audio and no serial port. [measured]

| | |
|---|---|
| Vendor / product | `0x16d0` / `0x0d40` |
| Strings | Tentacle Sync GmbH — Tentacle Sync E Mk2 |
| Device class | 0 (composite) |
| Interfaces | 2 × class 255 / sub 255 / proto 255 |

Both interfaces are vendor-specific. No USB Audio Class descriptor, so no
CoreAudio device ever appears; no CDC, so no `/dev/cu.*` either.

## Method

Everything above came from watching advertisements against a device whose
timecode and date were known, using `tentacle-ble --raw`, which dumps payloads and
marks which bytes changed. Fields announce themselves by how fast they tick: a
byte changing once a second is seconds, one changing 25 times a second at 25 fps
is frames.

```
[  1.221s] Ricki
    svc 0000fdac  22 05 19 09 22 33 06 a6 50
[  1.538s] Ricki
    svc 0000fdac  22 05 19 09 22 33 0e 97 45
                                    ^^ ^^ ^^
```

Two techniques did the work eyeballing could not. **GF(2) elimination** tested the
whole family of CRC-like checksums at once instead of guessing polynomials one at
a time. **Regression against host arrival timestamps** identified the microsecond
field, by asking which interpretation best predicts when a packet actually showed
up — then confirming the answer on a capture that had not been used to find it.

## Open questions

Each needs a device the observed one couldn't provide.

- **29.97 vs 30.** The rate is broadcast as a whole number, so the two are
  indistinguishable over the air. Set a device to 29.97 and compare byte 2 against
  a 30 fps device.
- **Drop-frame.** No flag appears anywhere in the payload; LTC has one, BLE seems
  not to. Watch byte 0 in drop-frame mode — the high nibble (2 vs 4) looks like it
  has room.
- **The 3.7 ms bias.** Real and consistent, origin unknown. Compare against a
  second unit; a transmit-path constant should be identical across devices.
- **Manufacturer bytes.** Capture while the battery discharges.
- **Date record bytes 2 and 6,** fixed at `00` and `02`, and its constant trailer.
  Change the date and see what moves.
- **Byte 1 as a length.** Find a record type with a different payload size.

---

No affiliation with Tentacle Sync GmbH. Observed against one Tentacle Sync E Mk2
at 25 fps on 2026-09-04.
