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

Last, the vendor GATT service was read and subscribed to on both boxes — 8,230
notifications across 38 connections — and the user drove a sync from the phone
app on cue, twice, the second time changing the frame rate to 24. **That is the
first observation at a second frame rate,** and it retires three inferences
below: byte 2 is the frame rate, frames run to fps−1, and the microsecond
counter scales as `1e6 / fps`. It also decoded one bit of the flags byte and the
last-sync timestamp the service reports. Nothing was written to the device.

## Four transports

| Transport | Rate | Precision | Availability |
|---|---|---|---|
| BLE advertisements | ~1.4–1.8/s | <1 ms per packet | Always on, no pairing |
| BLE GATT notifications | ~25/s, every frame | ~20 µs consistency | Needs a connection, which lasts 7 s |
| LTC on audio out | every frame | sample-accurate | Needs a real audio input |
| USB-C | — | — | Vendor-specific, undocumented |

The trade is rate against precision, and then against effort. A BLE
advertisement carries a microsecond stamp, so any single reading is excellent,
but they arrive under twice a second — nothing can be clocked from them without
interpolation. The vendor GATT service pushes the same reading every frame and
holds up far better against a host clock, but the box drops the connection after
about seven seconds, so a continuous stream means reconnecting all day. LTC
arrives continuously and is what you want for syncing to picture.

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

What byte 1 *does* mean was left open here as "the shape of a flags byte and not
evidence of what it flags". One of the bits is now pinned and the rest are at
least bounded.

**Bit 0 clear means a central holds a GATT connection.** [measured] This
document previously put the `0x7c` packets down to the bottom bits "flickering
on their own". They weren't flickering — something was connected. Tested three
times with the two boxes swapping roles, watching both while connecting to one:

| Subject | Byte 1 while connected | Flipped back at | Control box |
|---|---|---|---|
| Ricki | `0x7c` ×12 | 7.1 s | Liliana `0x7d` ×341, no change |
| Liliana | `0x7c` ×10 | 8.2 s | Ricki `0x7d` ×28, no change |
| Liliana | `0x7c` ×2 | 8.3 s | Ricki `0x7d` ×23, no change |

The subject dropped bit 0 for exactly as long as the connection lasted and set
it again when the box hung up — and the flip-back time matches the independently
measured connection life of about 6.6 s plus the time to connect, every time.
The control never moved. The subject-side counts are small (2 to 12 packets)
because a connected box advertises less, which is itself worth knowing.

**Bits 3–6 move while the app configures a box, and settle back.** [measured
that they move; [unknown] what they mean] A sync was performed from the phone
app while a passive scanner watched both boxes:

```
Ricki     0x7d -> 0x7c -> 0x15 -> 0x3d -> (30 s) -> 0x55 -> 0x7d
Liliana   0x7d ---------------> 0x3d -> (32 s) ----------> 0x7d
```

Both ended where they started. The intermediate values are not a monotonic
progression — bits 3 and 5 go off at `0x55` after being on at `0x3d` — so this
is not a progress counter being filled in. Note the first step on Ricki is
`0x7c`: the app connected, which is bit 0 doing what bit 0 does.

Every value ever observed, for whatever it is worth: `0x05`, `0x07`, `0x15`,
`0x3d`, `0x55`, `0x7c`, `0x7d`. **Bit 2 is set in all seven.** Bit 1 has been
seen set only in `0x07`. And the payload stayed nine bytes through every one of
them, which is byte 1 failing to be a length for the third separate time.

So: read bit 0 if you want to know whether something is connected, and do not
special-case the byte as a whole. [unknown]

### Timecode record — `0x22`

| Byte | Field | Encoding | Observed | |
|---|---|---|---|---|
| 2 | Frame rate | binary, whole fps | 25 and 24 | [measured] |
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
none above, against a frame-rate byte of 25.

That agreement used to be the main support for reading byte 2 as the frame rate
at all, which made it an inference from a single rate. It isn't any more. The
user set both boxes to 24 fps from the Tentacle app, and byte 2 went `0x19` →
`0x18` on both while the frames field started topping out at 23 instead of 24 —
all 24 values `0x00`–`0x17` and none above, over 4,542 readings. Two rates, two
boxes, the frame ceiling following the byte both times. **Byte 2 is the frame
rate.** [measured] The detail is in **The vendor GATT service** below, where the
same change also settles the microsecond scaling.

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

**Scaling by frame rate is measured, not inferred any more.** [measured] This
said, correctly at the time, that `1_000_000 / fps` microseconds per frame was
the natural reading of a 1 MHz counter and not something demonstrated at a
second rate, because only a 25 fps device had ever been seen.

A second rate has now been seen. At 24 fps the trailer's observed span grew from
39,705 to 41,646, against frame periods of 40,000 and 41,667 µs — it tracked the
frame rate to within 0.05%. A fraction-of-a-frame field could not do that, since
it would span the same fraction of 65,536 at any rate. **Scale by fps; do not
hardcode 40000.** Evidence in **The vendor GATT service** below.

Note what did *not* scale: the few-millisecond bias above stayed at about 3.6 ms
at both rates rather than growing with the frame period, so it is an absolute
offset in the counter's origin and not a fixed fraction of a frame.

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
box was plugged into USB-C and pulled out again three times over an hour, while
the other sat on battery as a control. Every manufacturer record from both boxes
was captured throughout (26,401 of them); these are all the changes:

```
   0.1s  Liliana   02 00 60 01 13      96%, on battery
 465.5s  Liliana   02 00 e0 01 13      plugged in — bit 7 up, still 96%
 557.1s  Liliana   02 00 e1 01 13      97%
 574.3s  Liliana   02 02 e1 01 13      byte 1 changes, 109 s late (see below)
 603.6s  Liliana   02 02 e2 01 13      98%
 650.3s  Liliana   02 02 e3 01 13      99%
 696.2s  Liliana   02 02 e4 01 13      100%
 738.2s  Liliana   02 02 64 01 13      unplugged — bit 7 down
 821.6s  Liliana   02 02 e4 01 13      plugged in again
 833.0s  Liliana   02 02 64 01 13      unplugged
1677.2s  Liliana   02 02 e4 01 13      and again
1715.8s  Liliana   02 02 64 01 13      unplugged
         Ricki     02 00 64 01 13      control: not one change in the hour
```

Bit 7 went up on plug-in and down on unplug, all three times, on the charging box
and never on the control. Meanwhile the low seven bits climbed 96 → 97 → 98 → 99
→ 100, one at a time with none skipped, over the 231 s the cable was in on the
first cycle. That independently confirms the low bits are a charge level, and
settles that the gauge tracks upwards as well as down:

```
   0x60   0 1100000      96%, on battery
   0xe2   1 1100010      98%, charging
```

**Mask before you range-check.** A charging device at 98% advertises `0xe2`,
which is 226. Read whole it is a nonsense percentage; range-checked against 100
before masking, the whole reading is discarded and the charge disappears from
your display at precisely the moment a box is plugged in. This is not
hypothetical — it is the bug the first version of `shokushu-ble` shipped with.

**That the scale is percent is a step less certain.** [inferred] 100 is the
largest value seen and 96 the smallest, so the top of the range is pinned and
everything below it is extrapolation. Percent is the natural reading of a gauge
that stops at 100, but only a real discharge would show whether it reaches 0
linearly, or at all. Nothing here has seen a box below 96.

**Byte 1 latches on something, and it isn't the charger.** [unknown] It read
`0x00` on both boxes for every capture until the first charge, when it became
`0x02` and stayed there — through all three unplugs and to the end of the hour,
while bit 7 came straight back down each time.

The timing rules out the obvious reading. Byte 1 did not change when the cable
went in: bit 7 flipped at 465.5 s and byte 1 was still `0x00` at 557.1 s, by which
point the battery had already gained a percent. It changed at 574.3 s, **109
seconds after charging began**, and never moved again. So it is not a
charger-detect line, and it is not simply a slower copy of bit 7. Something a
minute or two into a charge sets it, and nothing in the following seventeen
minutes cleared it. The control box, never charged, still reads `0x00`.

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

The three notifying characteristics are decoded in **The vendor GATT service**
below; `0dab144c` turns out to carry the timecode at the frame rate, which is
fifteen times the advertisement's. The write is still untouched — it is
presumably how the Tentacle app sets a device's time and name, and poking at it
blind is how a box ends up needing a factory reset.

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

## The vendor GATT service

The `0xfdac` service is the same 16-bit UUID the timecode is advertised under,
so the broadcast and this service are one service seen from two sides. That
prior turned out to be right: what the service pushes is the timecode record
with its header taken off, and much faster.

Everything in this section is read-only. Nothing was written to any
characteristic — see "The write characteristic" at the end for why, and for
what it would take.

Collected with `shokushu-gatt`, which connects, reads, subscribes, and logs
what arrives with a host timestamp. Corpus: 8,230 notifications over 380 s of wall
clock — about 250 s of it actually connected, for reasons the next subsection
is about — from one box, plus shorter runs on the second box, at **two** frame
rates. That is the first time anything in this document has seen a second rate.

### Connections last about seven seconds

**The box hangs up on its own, after around 6.6 s.** [measured] This is the
first thing to know about the service, because it shapes everything else.

| Run | Sessions | Link life |
|---|---|---|
| Ricki, 25 fps, subscribed | 17 | 4.44–6.81 s, mean 6.61 s |
| Ricki, 24 fps, subscribed | 21 | 3.27–6.81 s, mean 6.59 s |
| Liliana, subscribed | 2 single runs | 6.70 s, 6.75 s |

**It is not the traffic.** [measured] The obvious explanation is that
notifications at 33/s overrun something. They don't: a session subscribed to
nothing at all, doing one read a second, died just the same — last successful
read at 6.2 s, first failure at 7.1 s. Both boxes do it, so it is not one unit.
A client that does nothing and a client taking 220 notifications get the same
seven seconds, which is the signature of a timer and not of flow control.

**Scanning while connected makes it worse.** [measured] With the scan held up
through the connect, service discovery itself failed every time — the link died
before a single characteristic could be read. Restarted after subscribing, the
usual seven seconds. So the advertisement can be watched *or* the service can
be, and a tool that wants both gets a much shorter look at the second.

The practical consequence: **anything that wants a sustained stream off this
service has to reconnect continuously.** `shokushu-gatt --reconnect` does, and
gets 17 to 21 connections a minute. Whether the app avoids this by writing a
keepalive is a reasonable guess and nothing more; it is the sort of thing the
write characteristic might be for.

**And a box stops accepting connections after about thirty of them.**
[measured] Seen on both units in one afternoon of reconnect-driven capture:
after roughly 30 sessions in five minutes, `connect` stopped being answered at
all and every attempt timed out, indefinitely. It is connections specifically —
the box goes on advertising normally throughout, and a passive scanner sees no
change. One box did it first, and the other did the same thing a few minutes
later under the same load, so it is not a single unit.

What clears it was not established. Note that a client with no timeout on
`connect` hangs forever here rather than reporting anything, since a run's
deadline is usually only tested between sessions; `shokushu-gatt` bounds the
wait at 15 s for that reason. This is a plausible explanation for a Tentacle
"going off the air" after heavy tooling and is worth ruling out before
suspecting hardware.

### `0dab144c` — the timecode, headerless

**Seven bytes: the advertisement's timecode record with the two header bytes
removed.** [measured]

```
   19 0c 24 33 05   67 51
   ~~~~~~~~~~~~~~   ~~~~~
   |                `-- microseconds into the frame, big-endian u16
   `-- frame rate, hours, minutes, seconds, frames — binary, as in the advert
```

Same five fields in the same order with the same encoding as bytes 2–6 of a
`0x22` advertisement, and the same trailer as bytes 7–8. No record type, no
flags byte. Every payload in the corpus was seven bytes — 8,230 of them across
two frame rates, with no other length and no date record ever appearing here.

The field boundaries fall out of how fast each byte moves, which is the method
the advertisement was decoded with. Over one 180 s run:

| Byte | Distinct values | Reading |
|---|---|---|
| 0 | 1 (`25`) | frame rate |
| 1 | 1 (`12`) | hours |
| 2 | 4 | minutes |
| 3 | 60, all of `0`–`59` | seconds |
| 4 | 25, all of `0`–`24` | frames |
| 5–6 | many | microseconds |

**One sample per connection event, not per frame.** [measured] Within a session
the rate is 32.2–33.2 notifications/s, median 32.7 — faster than 25 fps. The
gaps say why. Of 3,671 gaps inside sessions, 580 were 0–1 ms — two
notifications delivered in the same event — and of the remaining 3,091,
**3,076 fell within 4 ms of a multiple of 30 ms**: 2,654 at one interval, 317
at two, 78 at three and a thin tail out to eight. Only 15 were anywhere else.
That is a 30 ms connection interval with the occasional event missed, and the
device stamping the clock whenever it fills a packet.

Because 30 ms is shorter than a 40 ms frame, some frames get sampled twice and
none get missed. Of 3,688 notifications, 2,762 carried distinct timecodes, 918
appeared twice and 4 three times — and **of the 922 repeats, 899 carried a
different microsecond trailer.** They are two genuine readings inside one
frame, not retransmissions of one. The remaining 23 were byte-identical, so a
reader that de-duplicates on payload equality is still doing something, just
much less than on the advertising side.

The distinct-timecode rate is 23.9–24.9/s against a 25 fps device. **Every
frame arrives.** That makes this by a wide margin the best clock the box
publishes:

| Source | Fresh readings | Cost |
|---|---|---|
| Advertisement | 1.4–1.8/s | none, passive |
| `0dab144c` | ~25/s, every frame | a connection that dies every 7 s |

### The microsecond field, at two frame rates

The advertisement section identifies bytes 7–8 as a microsecond count by
regressing against host arrival times, and marks the *scaling* — one frame
being `1_000_000 / fps` of them — as an inference, since only a 25 fps device
had ever been seen. **A second frame rate has now been seen, and it settles
it.** [measured]

The user set both boxes to 24 fps from the Tentacle app between two captures.
Nothing else was changed.

| | 25 fps | 24 fps |
|---|---|---|
| Notifications | 3,688 | 4,542 |
| Frame-rate byte | `0x19` | `0x18` |
| Frames observed | `0`–`24`, all 25 | `0`–`23`, all 24 |
| **Trailer span** | **39,705** | **41,646** |
| One frame in µs | 40,000 | 41,667 |

The span of the trailer grew when the frame rate fell, and grew to within 0.05%
of the new frame period. A fraction-of-a-frame field could not do that — it
would span the same fraction of 65,536 at any rate. **The counter is absolute
microseconds on a ~1 MHz clock, and one frame's worth of it is `1e6 / fps`.**
Scale by the frame rate; do not hardcode 40,000.

The regression agrees, and much more sharply than on the advertising side.
Median-of-session absolute residual of reconstructed device time against host
arrival time:

| Interpretation | 25 fps (17 sessions) | 24 fps (21 sessions) |
|---|---|---|
| Ignore the trailer | 10.00 ms | 11.83 ms |
| Fraction of a frame, ÷65536 | 3.90 ms | 4.61 ms |
| **Microseconds, ÷1e6** | **0.020 ms** | **0.029 ms** |

Twenty microseconds, against 0.61–0.73 ms for the same test on
advertisements. The
reason is that a notification and its delivery are locked to the same connection
event, so the transmit latency barely varies, where an advertisement is caught
on whichever of three channels the scanner happened to be on. Ignoring the
trailer costs a quarter of a frame at both rates, which is the quantisation you
would predict.

**What that number does and does not say.** It measures how *constant*
(device time − host time) stays within a session, not absolute accuracy — a
fixed error would not show up at all. Twenty microseconds of consistency over
200-odd samples is the field being a microsecond counter and very little else,
but it is not a calibration.

Little-endian is ruled out flat: read that way the values span 65,400 of the
available 65,536, which is what an arbitrary byte pair looks like and not a
counter into a 40 ms frame.

### The few-millisecond bias is a fixed time

The advertisement section records a floor around 3.7 ms in the trailer rather
than 0, calls the origin unknown, and asks whether a second unit would show the
same — a transmit-path constant being the guess. There is now data from two
transports, two units and two frame rates.

| Source | n | min | max |
|---|---|---|---|
| Ricki notify, 25 fps | 3,688 | 3,656 | 43,361 |
| Ricki notify, 24 fps | 4,542 | 3,624 | 45,270 |
| Ricki notify, third capture | 222 | 3,715 | 41,145 |
| Ricki advert, same capture as its notify | 299 | 3,686 | 43,361 |
| Liliana notify | 220 | 3,749 | 43,356 |
| Liliana advert, same capture | 322 | 3,951 | 43,532 |

Two things follow. **The floor is not specific to the advertising path** — it
is the same on a GATT notification, and the two paths share nothing but the
device's clock, which makes a transmit-path constant the less likely reading.
And **it did not scale with the frame period**: 3,656 µs at 25 fps and 3,624 µs
at 24 fps, where a fixed *fraction* of a frame would have moved to about 3,800.
So it is an absolute offset of roughly 3.6 ms in the counter's origin. [measured]

The caveat matters and cuts against reading much into the exact value: a
minimum over n samples is a biased estimator of a floor and creeps downwards as
n grows, which is visibly what happens above — n=220 gives 3,749 and n=3,688
gives 3,656. What is consistent across all six is the magnitude, not the number.

### A round trip bounds the offset; a broadcast cannot

Everything above measures the device's clock against host arrival times, which
cannot separate a transmit-path constant from a flight time from a stack delay
— they are one quantity to anything that only listens. **An ATT read is a round
trip, and that is what an advertisement structurally cannot be.** [measured]
Stamping the host clock either side of `peripheral.read()` on `0dab144c` gives
NTP's four timestamps, with the timecode in the response standing in for the
device's own two.

Write `a = T - t0` and `b = t1 - T` for a read that went out at `t0`, came back
at `t1`, and carried device time `T`; write `θ` for the device's clock minus
this host's. Then `a = d_out + θ` and `b = d_ret - θ` for the two one-way
delays. Neither delay is knowable on its own, but both are elapsed times and so
both are at least zero, which gives `θ ≤ a` and `θ ≥ -b` **for every single
sample**. The tightest pair over a run brackets the offset.

That deliberately does not halve the round trip. The two legs are not equal
here — a request waits for the next connection anchor and a response does not —
so halving would put the answer at the middle of the bracket and take the
asymmetry on as bias, of the same order as the effect being measured.

Collected with `shokushu-gatt --no-subscribe --scan --phase`, reduced by
`analysis/gatt_phase.py`. Two boxes at 24 fps, on separate runs:

| | Sun | Ricki |
|---|---|---|
| Round trips | 2,661 over 259 s, 30 connections | 1,206 over 146 s, 17 connections |
| Shortest round trip | 30.03 ms | 29.62 ms |
| Under one 30 ms interval | 0 of 2,661 | 2 of 1,473 |
| **Bracket on θ, per 30 s block** | **3.12–3.64, median 3.40** | **3.17–3.82, median 3.34** |
| Bracket pooled, drift removed | 3.11 ms | 3.20 ms |
| Drift from the block midpoints | +7.7 ppm | +8.8 ppm |

**Two units, measured a few minutes apart, agree to within a tenth of a
millisecond on the bracket width.** [measured] That is the figure being claimed,
and it repeating across boxes is most of the reason to believe it.

**The shortest round trip is one connection interval and not less.** [measured]
30.03 ms against the 30 ms interval the notification gaps establish. A read
handed to the controller waits for the next anchor point, so the reads are
dithered by a prime number of microseconds to sweep `t0` across that grid —
polled on a fixed cadence, `t0` can sit at one phase of it for a whole session
and the floor never gets sampled.

**The bracket is much narrower than the shortest round trip**, because the two
minima are achieved by different samples: `min(a) + min(b) ≤ min(a + b)`, with
equality only if one sample is best on both legs, and none is. So a 30 ms round
trip still bounds the offset to about 3.4 ms.

**A negative bracket is the arithmetic catching a bad sample**, and it happened
once. `min(a) + min(b) = min(d_out) + min(d_ret)`, a sum of two elapsed times,
so it cannot be below zero — a block that comes out negative contains at least
one sample whose device stamp predates the request that returned it. Ricki's
last block did, at the moment the box stopped answering connections: one read
with an 88 ms round trip and a device stamp 27 ms out of place. The other four
blocks were unaffected and agree with each other to 0.65 ms. This is worth more
than the sample it rejects: **every figure here is a minimum, so a single
impossible sample would otherwise silently become the answer**, and the sign
test finds it without needing to know what the answer should have been.

Drift has to come out before any of this is pooled, and not for the usual
reason. `a` rises with a positive drift while `b` falls, so `min(a)` is taken
from early in a capture and `min(b)` from late — the two bounds then constrain
*different* values of `θ` and the interval between them is spuriously narrow
rather than spuriously wide. Pooled raw over this capture the bracket reads
1.75 ms, which is not a bound on anything.

The drift falls out as a by-product: the block midpoints move at **+7.7 ppm**
over the capture, scattering 0.12 ms rms about a straight line. That is the same
quantity `shokushu-ble --drift` measures from one-way anchors, arrived at from
round trips, and the two agree to within the spread either reports.

### One connection brackets the offset; the advertisements are the slow part

The capture above took 15 connections over 314 s, and did not need to for the
bracket. **One 6.6 s connection yields about 81 round trips, and a bracket is a
minimum over them** — a minimum over fewer samples sits *higher*, so a short
capture reports a bracket that is too wide rather than too narrow. [inferred]
That is the safe direction. What the extra connections buy is the two
diagnostics: the connected-against-free-running comparison needs about 4.5
connected advertisements per connection and so a dozen connections to reach a
useful matched count, and a drift rate fitted from block midpoints needs three
separated blocks of round trips. Both are worth having and neither changes the
offset, which is why `shokushu-ble --jam` opens one connection and
`shokushu-gatt --phase --reconnect` still exists for the rest.

**But `θ` measured in a 6.6 s window does not stay valid for long against
advertisements pooled afterwards.** [inferred] `a` carries `θ` as it was during
the connection and `b` carries `−θ` whenever the advertisement landed, so a
later advertisement has a smaller `b` and wins the floor for a reason that is
drift rather than delivery. With one connection there is a single block and so
no slope to de-trend with. The quantity being eaten into is not the 3.4 ms
round-trip bracket but the **2.05 ms** the two relevant floors leave — 1.7 ms
outbound on a read, 0.35 ms on an advertisement — since that sum is what the
advertisement's staleness bracket spans:

| Advertisements pooled for | Drift at 8.6 ppm | Effect on the offset |
|---|---|---|
| 30 s | 0.26 ms | quoted 0.13 ms low |
| 60 s | 0.52 ms | quoted 0.26 ms low, on a true ~1.02 ms |
| 240 s | 2.06 ms | exceeds the 2.05 ms; the bounds cross |

**Past about four minutes the pooled bounds on `θ` cross and the capture is
refused** rather than reducing to a plausible number near zero — the arithmetic
catching its own limit, the same way the negative-bracket check does.
[inferred] `shokushu-ble --jam` pools for 60 s for that reason. All of this is
arithmetic over the measured floors and the measured drift rather than a fresh
measurement, which is why it is marked inferred.

### Reads and notifications cannot be told apart on macOS

**A read taken while subscribed is not a round trip.** [measured] CoreBluetooth
delivers a Read Response and a notification through the same delegate callback,
so a pending read is resolved by whichever arrives first — and the device pushes
a notification every connection event. The value is genuine and its timing is
fiction.

Counting round trips shorter than one connection interval, which a real one
cannot be:

| Capture | Under 30 ms | Shortest |
|---|---|---|
| `--no-subscribe`, n=443 | 0 (0.0%) | 30.26 ms |
| `--no-subscribe`, n=2,661 | 0 (0.0%) | 30.03 ms |
| `--no-subscribe`, n=1,473 | 2 (0.1%) | 29.62 ms |
| subscribed, n=3,388 | 2,833 (83.6%) | 0.031 ms |

Thirty-one microseconds is not a Bluetooth round trip. The bracket computed
from a subscribed capture comes out **negative**, which is the arithmetic
saying so: a negative width means the device's stamp did not fall between the
two host stamps, and here it did not because the stamp arrived before the read
that "returned" it was issued. So `--phase` wants `--no-subscribe`, and warns
when it doesn't get it. A subscribed capture is not wasted — the notifications
in it are real — but its reads bound nothing.

This is a property of the host stack and not of the device, and it is worth
knowing for anything that reads and subscribes to the same characteristic:
**the value is right and the latency is not.**

### What an advertisement costs, and why the first answer was wrong

The offset that matters to a clock built on advertisements is the
advertisement path's, and a GATT round trip measures the connection path. They
are different journeys. But every stream gives `b = arrival − device stamp
= d − θ` for its own delivery delay, and `θ` is common to all of them, so
differencing two floors cancels it and leaves delivery alone.

**The trap is that connecting to a box changes the thing being measured.**
[measured] A box holds a link *and* advertises, and the advertising loses. It
does not merely slow down from 1.4–1.8 fresh readings a second to 0.6 — each
advertisement also lands **9.5 ms later relative to the device's own stamp**.
Measured on one box, matched at 67 samples either side so that a thinner sample
cannot masquerade as a slower path:

| Advertisements | n | Delivery floor |
|---|---|---|
| While a link was up | 67 | +9.53 ms |
| Between sessions, box free-running | 67 | 0 (reference) |

So the honest measurement needs the box *not* connected, which sounds like it
rules out the round trip that supplies `θ`. It doesn't: alternate. Connect,
take round trips, disconnect, let the box advertise normally, reconnect. `θ`
carries across the gaps on the measured drift — 8.6 ppm here, so 0.09 ms across
a 10 s rest, negligible against what is being measured. `shokushu-gatt
--rest-ms` exists for this, and the phase capture records whether a link was up
when each advertisement landed.

With `θ` bracketed by the round trips, the staleness of a stream's *least
delayed* reading — how far behind the device's real clock it was when it landed,
which is the error left in a clock that anchors on the best reading it sees — is
bounded by `min(b) − min(b_best)` below and `min(b) + min(a_read)` above, where
`min(b_best)` is the floor of whichever stream reached the host soonest. One
box, 24 fps, 1,212 round trips and 601 advertisements over 314 s in 15
connections with a 10 s rest between them:

| Stream | n | Staleness of the least delayed | Of a frame at 24 fps |
|---|---|---|---|
| **Advertisement, free-running** | **534** | **0 to 2.09 ms** | **0.00–0.05** |
| GATT read response | 1,212 | 1.35 to 3.43 ms | 0.03–0.08 |

**A clock anchored on the least delayed advertisement sits at most about 2 ms
behind the device's own** — a twentieth of a frame at 24 fps, and not
distinguishable from no delay at all. [measured] That is the figure the
`freerun` module docs called unmeasured.

The zero is by construction rather than by measurement: the advertisement
stream turns out to give the tightest lower bound on `θ` of any stream here, so
it reads zero and everything else is measured against it. **The content is the
upper bound.** The advertisement path is also *faster* than the read-response
path by 1.35 ms, which is what one would expect — a response waits for the next
connection anchor and a broadcast does not.

The ~3.6 ms origin bias in the microsecond counter is inside these figures
rather than beside them: a round trip bounds the total a reading is behind by
and cannot take that total apart.

**An earlier version of this section reported 6 to 9 ms**, from two captures
whose advertisements were *all* taken while a link was up. The number was a
measurement of the observer. It is recorded here because the mistake is an easy
one to repeat: the tool that supplies the reference is the same tool that
perturbs the signal, and nothing in the capture looks wrong when it happens.
The check that caught it — comparing the two populations at matched sample
counts — costs nothing and should be run on any figure of this kind.

### Three boxes agree to a third of a millisecond, passively

Two boxes' offset against each other can be measured without connecting to
either, which nothing above can do for a box on its own. For each reading,
`host arrival − device stamp` is that box's delivery delay minus its own clock
offset; the delivery *floors* are the same quantity for two identical radios at
the same distance from one host, so differencing two boxes' floors cancels the
delay and the free-running host clock together and leaves how far apart the two
boxes are. `analysis/box_agreement.py` does it, at matched sample counts for the
usual reason.

Ricki, Liliana and Sun at 24 fps, over one 90 s passive capture — 811 readings,
matched at n=263 per box, with no connection opened to any of them before or
during: [measured]

| Box | Against the earliest | Of a frame at 24 fps |
|---|---|---|
| Sun | 0 (reference) | 0.000 |
| Ricki | +0.144 ms | +0.003 |
| Liliana | +0.327 ms | +0.008 |

All three floors had stopped moving by the end — the last convergence step
shifted each by under 0.1 ms, against the 0.33 ms being reported — so the spread
is the boxes and not the sample size. **Three boxes jam synced from the same
master agree to about a third of a millisecond, a hundredth of a frame at
24 fps.**

The equal-floors assumption is the one thing carrying this, and it is the reason
for a minimum rather than a mean: a mean would carry each box's whole delay
distribution, which differs with signal strength and how often it is heard from,
while a floor is the best case of a path and the best case of two like radios
should agree. The three RSSIs here spanned −29 to −39 dBm and the answer did not
order itself by signal strength, which is weak evidence for the assumption and
not a test of it.

This is the baseline the desync question under **Open questions** wants. It says
nothing yet about whether connecting moves a box — that needs the same
measurement again with a connection in between.

### The connection interval is not negotiable from macOS

**Nothing here can ask for a shorter one.** [measured] `btleplug` 0.13 declares
`Peripheral::connection_parameters` and `request_connection_parameters`, and
both default to `NotSupported`; only the WinRT backend implements them. The
CoreBluetooth and BlueZ backends implement neither, which matches CoreBluetooth
not exposing connection parameters to a central at all. So the 30 ms interval
is a given on this host, and with it the 30 ms floor under a round trip. A
7.5 ms interval would cut the quantisation fourfold and there is no route to
one from here.

### `0dab1280` — device state, including the last sync time

**Twenty-four bytes, length-prefixed, zero-padded, and it does not notify.**
[measured] It pushed nothing across 38 connections in 380 s of watching.
Between syncs it does not change either: 50 reads over one 180 s run and 82 over
another returned byte-identical values every time.

It changes when the box is synced. Four samples, two boxes either side of one
observed sync:

```
Ricki    before   0d 6c 00 0c 01 00 53 19 00 00 04 09 1a 0b 1c 28 00×8
Ricki    after    0d 6c 00 0c 01 00 09 18 00 00 04 09 1a 0c 2f 15 00×8
Liliana  before   0d 10 00 01 02 00 00 00 00×16
Liliana  after    0d 6c 00 10 01 01 09 18 00 00 01 0c 2f 11 04 09 1a 0c 2f 14 00×4
```

**Byte 3 is a length.** [inferred] It counts the bytes after itself, and the
arithmetic is exact on all four samples — 12, 12, 1 and 16 against payloads of
12, 12, 1 and 16 bytes, with the remainder of the 24 zero-padded. Four samples
in three shapes is thin, and this document has been wrong about a length byte
before, in the other direction: read this as the reading that fits, not as
settled.

Laying the payload out on that basis:

```
   0d   6c   00   10   01 01   09   18   00 00   01 0c 2f 11   04 09 1a 0c 2f 14
   ~~   ~~   ~~   ~~   ~~~~~   ~~   ~~   ~~~~~   ~~~~~~~~~~~   ~~~~~~~~~~~~~~~~~
   |    |    |    |    |  |    |    |    |       |             `-- dd mm yy hh mm ss
   |    |    |    |    |  |    |    |    |       `-- `n` four-byte items
   |    |    |    |    |  |    |    |    `-- always 00 00
   |    |    |    |    |  |    |    `-- frame rate
   |    |    |    |    |  |    `-- moved 0x53 -> 0x09 on a sync  [unknown]
   |    |    |    |    |  `-- n, the item count
   |    |    |    |    `-- always 01
   |    |    |    `-- length of everything after this byte
   |    |    `-- always 00
   |    `-- 0x6c on both boxes after a sync, 0x10 on one before  [unknown]
   `-- always 0d
```

**The last six payload bytes are the date and time of the last sync, binary
`dd mm yy hh mm ss`.** [measured] This is the one field an intervention pinned
down. Both boxes were read, the user synced them from the phone app while a
passive scanner watched, and both were read again:

```
Ricki     04 09 1a  0b 1c 28   ->   04 09 1a  0c 2f 15      4 Sep 26, 11:28:40 -> 12:47:21
Liliana   (zeros)              ->   04 09 1a  0c 2f 14      4 Sep 26,             12:47:20
```

The scanner puts the app's activity on both boxes between 12:46:46 and 12:47:20
by the devices' own broadcast timecode, and the two boxes wrote 12:47:21 and
12:47:20. Ricki's previous value, 11:28:40, matches the earlier sync from that
morning. Date first, then time, both plain binary — not BCD like the
advertisement's date record, which is worth knowing since `1a` is 26 in binary
and not a valid BCD digit pair at all.

**The frame rate is in there too, and it followed the app.** [measured] Byte 7
went `0x19` → `0x18` on Ricki and reads `0x18` on Liliana, matching the 25 → 24
change the user made in the same sync, and matching the frame-rate byte in both
the advertisement and `0dab144c`.

Two things are unexplained. Byte 6 moved `0x53` → `0x09` on Ricki and reads
`0x09` on Liliana after the sync, and nothing here says what it counts.
[unknown] And **Liliana's record was nearly empty before the sync** — length 1,
a single payload byte `0x02`, no timestamp — where Ricki held a timestamp from
that morning. If both boxes were synced together that morning, both should have
carried it. Liliana is the box that spent the morning on a charger, so a power
cycle clearing the record would fit, and so would several other things.
[unknown]

### `0dab2496` — twenty-four zero bytes

**Nothing, throughout.** [measured] A negative result, and worth writing down
so nobody spends an afternoon on it. It read as 24 zero bytes on both boxes in
every session; it notified nothing across 38 connections in 380 s of watching;
and it was unmoved by a frame-rate change, a sync from the phone app, and a
charge cycle. 132 reads, one distinct value.

### The write characteristic

`0dab17e4` advertises `WRITE` and nothing else — no read, no notify. There is
no way to observe it from a connected client, and **nothing was written to it.**

What the app puts there is the interesting question and this document is not
going to guess at it. Two of the things a sync does are now visible from the
outside — the clock is set, and the frame rate and a sync timestamp land in
`0dab1280` — which constrains the payload without revealing it.

Capturing it needs a sniffer on the phone↔box link, since a Mac cannot see
traffic between a phone and a device it isn't part of:

| Route | Needs |
|---|---|
| Tentacle Sync Studio on macOS + PacketLogger | the macOS app, and Additional Tools for Xcode |
| nRF52840 + nRF Sniffer into Wireshark | the dongle |
| Android "Bluetooth HCI snoop log" | an Android phone with the app on it |
| iOS Bluetooth debug profile + sysdiagnose | fiddlier, but no extra hardware |

None was available for this work: no Tentacle app and no PacketLogger on this
machine, no dongle, no Android handset. So the write side stays unknown, which
is a better answer than invented bytes.

### Descriptors

**No characteristic carries a user description.** [measured] The only descriptor
on any of the three notifying characteristics is a Client Characteristic
Configuration (`0x2902`), which is just the subscribe bits. There is no `0x2901`
Characteristic User Description anywhere in the vendor service, so the device
names nothing for you.

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
timecode and date were known, using `shokushu-ble --raw`, which dumps payloads and
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
  a 30 fps device. Two whole rates, 24 and 25, have now been seen and byte 2
  carried both; a fractional rate is the remaining case.
- **Drop-frame.** No flag appears anywhere in the payload; LTC has one, BLE seems
  not to. Watch byte 0 in drop-frame mode — the high nibble (2 vs 4) looks like it
  has room.
- **The 3.6 ms bias.** Narrowed, not solved. It is on the GATT notification path
  as well as the advertising one, on both units, and it did **not** scale when the
  frame rate changed — so it is an absolute offset in the counter's origin rather
  than a transmit-path constant or a fixed fraction of a frame. What sets it is
  still unknown. A device on a different firmware revision is the next test.
  Note that the round-trip measurement above does not settle this: it bounds the
  *total* a reading is behind by, this bias included, and deliberately does not
  take the total apart.
- **How much of the advertisement's staleness is flight and how much is origin.**
  The round trip bounds the sum and cannot split it, because both halves are
  fixed and a bracket only ever sees the sum. Splitting them wants a second
  transport whose latency is independently calibratable, which is what LTC on
  the audio output is: sample-accurate, and measurable end to end against a
  known signal. Read LTC and BLE off the same box at once and the audio path
  becomes the reference. Blocked on an input — see **Getting it into a Mac**.
- **Tightening the advertisement floor.** The free-running figure reads zero at
  its low end by construction rather than by measurement, so what is quoted is
  its upper end, and that end is the bracket — which the 30 ms connection
  interval floors and macOS gives no way to shorten. The honest fix is not a
  cleverer estimator, since a minimum has no unbiased form to reach for; it is
  either a much longer capture or a transport that is not Bluetooth.
- **Whether connecting to a box knocks it off a shared timeline.** [unknown]
  Two boxes have been observed out of sync after sessions of GATT work, and
  nothing here writes to a box — `0dab17e4` is left alone and an ATT read is
  what the round trips use. Three candidates, none tested. A connection could be
  re-jamming the box, though the evidence is against it: `0dab1280` holds the
  time of the last sync and stayed byte-identical across 50 reads in one run and
  82 in another, over many reconnects. Or the recovery could be the cause rather
  than the connection — a box wedged by rapid reconnects, then power-cycled,
  comes back unjammed. Or it could be ordinary drift, since two boxes 8.6 ppm
  apart separate by 0.74 s a day, about 18 frames at 24 fps. The clean test is
  differential and cheap: measure two boxes' offset against each other
  passively, where the host clock cancels, connect once to one of them, and see
  whether that offset moved. **Half of that test is now done.** The passive
  measurement exists — `analysis/box_agreement.py`, and **Three boxes agree to a
  third of a millisecond** above has the baseline: Ricki, Liliana and Sun within
  0.33 ms of each other with nothing having connected to any of them. What is
  outstanding is the second half, a repeat of that capture with one connection
  in between, which `shokushu-ble --jam --name <box>` is exactly.
- **The battery scale below 96.** Byte 2 of the manufacturer record is a charge
  level and 100 is its top, but no box has been watched below 96. Run one flat
  and see whether it reaches 0, and whether it gets there linearly.
- **What byte 1 of the manufacturer record latches on.** `0x00` until a box is
  first charged, `0x02` from then on. It lagged the plug-in by 109 s and never
  came back down, so it is neither a charger-detect line nor a slow copy of the
  charging bit. Reboot a device that reads `0x02` and see whether it clears; if it
  does, it's "charged since boot" and not something about the battery.
- **The remaining manufacturer bytes,** `02` and `01 13`. Unmoved by a second
  device, a charge cycle and a firmware-level app sync alike. Compare against a
  device on a different firmware revision.
- **Date record bytes 2 and 6,** fixed at `00` and `02`. Change the date and see
  what moves.
- **What byte 1's remaining bits mean.** Bit 0 is now known — it is clear while
  a central holds a GATT connection. Bits 3–6 move while the app configures a box
  and settle back where they started, non-monotonically, so they are not a
  progress counter. Bit 2 has been set in all seven values ever seen and bit 1 in
  only one. Toggle app settings one at a time and watch which bit follows.
- **Date record byte 8.** `00` in 112 records of 124 and twelve other values
  once each. A later capture added five more distinct values on one box
  (`0x57`, `0xcf`, `0xef`, `0xf0` alongside `0x00`) in 47 records, which keeps
  the pattern of "mostly zero, otherwise never the same twice" and still
  explains nothing. Capture across a date change and across midnight.
- **The write characteristic, `0dab17e4`.** The whole other half of the protocol.
  Write-only, so it cannot be observed from a connected client; it needs a
  sniffer on the phone↔box link. See **The write characteristic** above for the
  four routes and why none was available here.
- **What `0dab1280` byte 6 counts.** It moved `0x53` → `0x09` on one box across a
  sync and reads `0x09` on both afterwards. Sync twice in a row and see whether it
  moves again.
- **Why `0dab1280` was nearly empty on one box.** Length 1 and no timestamp on
  Liliana before the sync, where Ricki carried that morning's. Liliana had spent
  the morning on a charger, so a power cycle clearing the record would fit. Reboot
  a box with a timestamp in it and read the characteristic again.
- **Whether the connection would live longer if the client wrote something.** The
  box hangs up after about 6.6 s regardless of what a read-only client does. A
  keepalive on the write characteristic is a plausible reason the app doesn't
  suffer this, and a sniffer capture would show it.

---

No affiliation with Tentacle Sync GmbH. Observed against two Tentacle Sync E Mk2
units, at 25 and 24 fps, on 2026-09-04.
