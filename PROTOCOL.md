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

A second device, on the same firmware and also at 25 fps, was added later that
day: a 45-minute two-box capture, a GATT probe of both, and two plug/unplug
cycles on one of them with the other held on battery as a control. It lifts the
single-device caveat only where it says so — the battery byte, the charging bit
and the GATT tree.
Two boxes of the same revision still cannot separate a firmware constant from a
field that never moved, and nothing here has yet seen a second frame rate.

Both boxes were then connected to the Tentacle phone app and synced, and a
header byte changed on both. Two claims below are corrections rather than
additions — byte 1 is not a length, and the date record's trailer is not a
constant — and both had been believed on the strength of a byte that never
varied in a corpus too small to make it vary. Where a claim below rests on
"never observed to change", read it with that in mind.

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

Capture C, collected by a second implementation that hadn't seen the first, puts
the copies-per-reading count at {2: 53, 3: 2} over 55 distinct readings: no
singletons, and two readings that arrived three times. Across both captures, 125
of 130 readings arrived exactly twice, 2 three times and 3 once. That
distribution is itself the evidence for the three-channel explanation, since a
device that simply transmitted everything twice could never produce a 3. It also
sets the shape of the de-duplication: **compare payloads, don't assume pairs** —
a reader that discards every second advertisement desynchronises permanently on
the first triple it meets.

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
   22   7d   19 0b 25 28 15   5f c6
   ~~   ~~   ~~~~~~~~~~~~~~   ~~~~~
   |    |    |                `-- trailer, big-endian u16
   |    |    `-- data field, five bytes, meaning set by type
   |    `-- flags, meaning unknown — NOT a length
   `-- record type
```

| Byte 0 | Record | Share of traffic |
|---|---|---|
| `0x22` | Timecode | 337 of 348 |
| `0x42` | Date | 11 of 348 |

**Byte 1 is not a length.** [measured] This document said it was, marked
[inferred], on the grounds that it held `0x05` in every packet ever seen — which
is exactly the width of the data field that follows — and that no packet of
another size had turned up to contradict it. Both halves of that were true and
the conclusion was still wrong.

The two boxes were then connected to the Tentacle phone app to sync them, and
byte 1 came back `0x7d` on both. The packets stayed nine bytes. A parser reading
byte 1 as a length asks for 125 bytes of a nine-byte payload, rejects every
advertisement, and — this being the third failure mode, and the reason this
paragraph is worth its length — reports nothing at all, because a scanner with
no valid readings looks exactly like a scanner with nothing in range.

| | Byte 1 | Payload size |
|---|---|---|
| Before the app sync | `0x05` ×117, `0x07` ×2 | 9 bytes, always |
| After the app sync | `0x7d` ×2199, `0x7c` ×4 | 9 bytes, always |

Across 2,322 payloads either side of the change the size never moved, so byte 1
does not describe it. **Read the layout as fixed: two header bytes, a five-byte
data field, an optional two-byte trailer.** [measured]

What byte 1 *does* mean is unknown. The sync set bits 3–6 together (`0x05` →
`0x7d` is `|= 0x78`) and the bottom bit or two flicker on their own — two `0x07`
packets before, four `0x7c` after. That is the shape of a flags byte and not
evidence of what it flags, so don't special-case a value: `0x7c` alone shows
that whatever byte 1 is, today's value isn't stable either. [unknown]

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
   42   7d   00 26 09 04 02   a1 00
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

**The trailer here is not a microsecond count, and not quite a constant
either.** [measured] This document said it was a constant, on 11 date records
that were byte-for-byte identical. A larger corpus — 124 date records — splits
it: byte 7 is `a1` in all of them, and byte 8 is `00` in 112 and something else
in the remaining 12, with no value repeating (`12`, `1b`, `33`, `43`, `47`,
`54`, `67`, `8c`, `a1`, `b6`, `f2`, `f9`).

So whatever bytes 7–8 are here, they are not the microsecond-into-frame counter
of a timecode record — a date record names no frame — and they are not fixed.
One byte pinned and the other mostly-but-not-always zero is the signature of a
field this corpus is too small to have provoked. Nothing reads it. [unknown]

That correction is worth noting as a method point: 11 identical samples looked
like proof of a constant, and were not. It is the same mistake as byte 1 above,
found the same way — by capturing more.

### Manufacturer data

Advertised alongside the service data — same advertisement, different field, and
a different event to a scanner, which is why it is easy to miss.

```
company 0x043f    02 00 64 01 13
                        ~~ battery, with charging in the top bit
```

**Byte 2 is the battery level.** [measured] Two boxes were put in range of each
other and read differently in that byte and nowhere else:

```
Ricki     02 00 64 01 13      0x64 = 100
Liliana   02 00 61 01 13      0x61 =  97
```

Two devices disagreeing is suggestive but not a gauge — the byte could be a
serial number's last octet for all that shows. What settles it is that it moves,
in the right direction, on its own. Over a 45-minute capture with both boxes
sitting untouched, Liliana went `61` → `60` and stayed there, while Ricki held at
`64` throughout:

```
[  0.046s] Liliana    02 00 61 01 13
[  0.327s] Ricki      02 00 64 01 13
[160.773s] Liliana    02 00 60 01 13
```

Nothing else in either record changed at any point — not the four bytes around
it, not on either device. A byte that differs between two devices, decrements by
one on the one that isn't full, never rises, and sits at exactly 100 on the one
that is, is a charge level.

**Bit 7 of that byte means "charging".** [measured] This one was tested properly,
because unlike a discharge it needs no patience — the intervention is a cable. One
box was plugged into USB-C and pulled out again, twice, while the other sat on
battery as a control:

```
[  1.3s] Liliana   02 02 e4 01 13      charging
[ 19.9s] Liliana   02 02 64 01 13      unplugged
[103.3s] Liliana   02 02 e4 01 13      charging again
[114.7s] Liliana   02 02 64 01 13      unplugged again
         Ricki     02 00 64 01 13      control, unchanged throughout
```

Bit 7 went up on plug-in and down on unplug, both times, on the charging box and
never on the control. Meanwhile the low seven bits climbed: 96 on battery before
the cable went in, then 98, 99 and 100 while it was charging. (97 was not seen,
but the capture didn't start until after the plug-in, so it was most likely just
missed rather than skipped.) That independently confirms the low bits are a
charge level, and settles that the gauge tracks upwards as well as down:

```
   0x60   0 1100000      96%, on battery
   0xe2   1 1100010      98%, charging
```

**Mask before you range-check.** A charging device at 98% advertises `0xe2`,
which is 226. Read whole it is a nonsense percentage; range-checked against 100
before masking, the whole reading is discarded and the charge disappears from
your display at precisely the moment a box is plugged in. This is not
hypothetical — it is the bug the first version of `tentacle-ble` shipped with.

**That the scale is percent is a step less certain.** [inferred] 100 is the
largest value seen and 96 the smallest, so the top of the range is pinned and
everything below it is extrapolation. Percent is the natural reading of a gauge
that stops at 100, but only a real discharge would show whether it reaches 0
linearly, or at all. Nothing here has seen a box below 96.

**Byte 1 latches on something, and it isn't charging.** [unknown] It read `0x00`
on both boxes for every capture until one was first plugged in, when it became
`0x02` — and then stayed `0x02` through both unplugs, while bit 7 of the battery
byte came straight back down each time. So it isn't a charging flag; it is
something that got set and did not reset within the observation. "Has been on a
charger since boot" would fit, and so would several other things. The control box,
never charged, still reads `0x00`.

**The remaining three bytes are still unknown.** [unknown] `02` at byte 0 and
`01 13` at bytes 3-4 never moved at all: not between two devices, not across a
charge, not across the app sync. That is what a hardware or firmware constant
looks like — and equally what a field that simply never changed looks like. Both
boxes report the same firmware and hardware revisions over GATT (below), so two
of them cannot tell those apart. A device on a different revision would.

### GATT services

Everything above is passive: the device broadcasts it and a scanner listens.
Connecting reveals a little more, at the cost of no longer being passive.
[measured]

| Service | Contents |
|---|---|
| `0x180a` Device Information | model number, serial number, firmware and hardware revision, manufacturer name |
| `0xfdac` vendor | four characteristics, three `READ｜NOTIFY` and one `WRITE` |

```
0dab1280-2cb9-11e6-b67b-9e71128cae77   READ | NOTIFY
0dab144c-2cb9-11e6-b67b-9e71128cae77   READ | NOTIFY
0dab17e4-2cb9-11e6-b67b-9e71128cae77   WRITE
0dab2496-2cb9-11e6-b67b-9e71128cae77   READ | NOTIFY
```

The vendor characteristics are unexamined. Reading one is harmless enough, but
the write is presumably how the Tentacle app sets a device's time and name, and
poking at it blind is how a box ends up needing a factory reset.

Device Information reads, identically on both units:

```
manufacturer name    "Tentacle Sync GmbH"
hardware revision    "1.2 SYNCE2"
firmware revision    "H: 1.1.5 BT: 2.4.2"
serial number        twelve digits, e.g. "2205........"
model number         six bytes that aren't text, then a NUL
```

**There is no Battery Service.** [measured] No `0x180f`, and no Battery Level
characteristic `0x2a19` anywhere in the tree. The standard route to a charge
level does not exist on this device, which makes the manufacturer advertisement
above the only place it is published — and the better place anyway, since reading
it needs no connection.

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
- **The battery scale below 96.** Byte 2 of the manufacturer record is a charge
  level and 100 is its top, but no box has been watched below 96. Run one flat
  and see whether it reaches 0, and whether it gets there linearly.
- **What byte 1 of the manufacturer record latches on.** `0x00` until a box is
  first charged, `0x02` from then on, and it did not come back down on unplug the
  way the charging bit did. Reboot a device that reads `0x02` and see whether it
  clears; if it does, it's "charged since boot" and not something about the
  battery.
- **The remaining manufacturer bytes,** `02` and `01 13`. Unmoved by a second
  device, a charge cycle and a firmware-level app sync alike. Compare against a
  device on a different firmware revision.
- **Date record bytes 2 and 6,** fixed at `00` and `02`. Change the date and see
  what moves.
- **What byte 1's bits mean.** Answered in the negative — it is not a length —
  but not answered. Bits 3–6 went on together when the boxes were synced to the
  phone app, so toggle app settings one at a time and watch which bit follows;
  the bottom bits flicker on their own and want a long capture to correlate
  against anything.
- **Date record byte 8.** `00` in 112 records of 124 and twelve other values
  once each. Capture across a date change and across midnight.

---

No affiliation with Tentacle Sync GmbH. Observed against one Tentacle Sync E Mk2
at 25 fps on 2026-09-04.
