# tentacle

Reads SMPTE LTC timecode from an audio input — e.g. a Tentacle Sync E plugged
into a Mac.

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

## How it works

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

`cargo test` runs the decoder against synthesized LTC from a biphase-mark
encoder in the test module, covering several frame rates and sample rates,
minute rollover, drop-frame and user bits, an inverted signal, recovery after a
dropout, arbitrary buffer boundaries, and rejection of silence, tones and noise.
