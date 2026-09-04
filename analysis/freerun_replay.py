#!/usr/bin/env python3
"""Replays `freerun`'s clock over a capture, to compare anchoring strategies.

The clock in `src/freerun.rs` has constants in it — how long a batch of
readings is gathered over before the least delayed of them is taken as the
anchor, how much of an error to take out at once, how long a baseline the rate
is measured over — and picking those by argument rather than by measurement is
how you end up with a number nobody can defend. This replays a recorded capture
through the same model at several settings and prints what each one costs.

    cargo run --features scan,cli --bin shokushu-ble -- --json --seconds 1800 > capture.json
    python3 analysis/freerun_replay.py capture.json

No dependencies, and slow enough to notice on a long capture and not slow
enough to care.

# The reference clock

There is nothing here to check a clock against: both oscillators are free
running and the host is no more a reference than the device. What there is, is
the shape of the noise. Bluetooth delivery error is one-sided — an
advertisement can reach the host late and never early — so on a plot of device
frame position against host arrival time the device's real clock is the *top*
of the scatter and not its middle. The reference is a straight line fitted
through the least delayed reading of each 20 s bin, iterated a few times from
an ordinary least-squares start.

**The reference is fitted on even-numbered 60 s blocks and every figure below is
measured on the odd ones**, so no reading that scores a setting helped place the
line it is scored against. Without that the longer windows score well for the
circular reason that they anchor on nearly the points the line was fitted
through.

# What it assumes

**That the device's clock is linear over the capture.** Two crystals at roughly
constant temperature, over half an hour. It is not verified here, and it is the
assumption everything rests on: if a box's rate wanders, the reference is wrong
and so is every column.

**That the constant offset does not matter.** It cannot be recovered from a
one-way broadcast at all — see the `freerun` module docs — so every figure is
reported about its own mean, and none of them is an absolute phase.

# The columns

- `anchor ms` — the timing noise on a single anchor, recovered from the scatter
  of the per-baseline rate measurements: a baseline spans two anchors, so their
  errors add in quadrature and `sd_ppm * baseline / sqrt(2)` puts it back in
  milliseconds. **This is the column that compares settings fairly**, because
  unlike the raw ppm scatter it does not shrink just because a longer window
  stretched the baseline.
- `rms ms` / `peak ms` — how far the clock ran from the reference. Read `peak`
  and not `rms`: a straight-line reference rewards an estimator for ignoring its
  input and free-running, so `rms` falls monotonically with the window whether
  or not anything got better. `peak` is the visible artefact — the worst
  excursion a display would show.
- `shown ppm` — spread of the drift figure the `--drift` column would have
  displayed, and the range it wandered over.
- `rate err` — how far the rate estimate sat from the reference, and the half a
  frame that implies. Computed from the rate rather than by running the
  reference out past the capture, which would be extrapolating a line well
  beyond anything it was fitted to.

The last section differences the two boxes. Both are measured against the same
host clock, so a rate they share is common-mode and cancels; what is left is
what they do relative to each other.
"""

import collections
import json
import math
import sys

# Ported from src/freerun.rs, and has to be kept in step with it by hand.
SLEW_GAIN, SNAP, HOLDOVER = 0.5, 0.5, 5.0
RATE_BASELINE, RATE_GAIN, MAX_RATE_ERROR = 10.0, 0.25, 500e-6

# Blocks the reference is fitted on and evaluated over, alternating.
BLOCK = 60.0
# Bin the envelope takes its least-delayed reading from. Long enough to hold a
# few readings at 1.4-1.8 fresh ones a second, short enough for a few dozen bins.
ENVELOPE_BIN = 20.0
WINDOWS = [None, 0.5, 1.0, 2.0, 3.0, 5.0, 8.0]


def load(path):
    """Capture to {device: [(host seconds, frame position)]}, and frame rates."""
    devices, fps = collections.defaultdict(list), {}
    for line in open(path):
        if not (line := line.strip()):
            continue
        d = json.loads(line)
        f = d["fps"]
        whole = ((d["hours"] * 60 + d["minutes"]) * 60 + d["seconds"]) * f + d["frames"]
        # Same arithmetic as Timecode::frame_position: whole frames since
        # midnight plus the sub-frame field as a fraction of one.
        devices[d["device"]].append(
            (d["host_micros"] / 1e6, whole + d["subframe_micros"] * 1e-6 * f)
        )
        fps[d["device"]] = f
    for name in devices:
        devices[name].sort(key=lambda point: point[0])
    return devices, fps


def lsq(points):
    n = len(points)
    mx = sum(p[0] for p in points) / n
    my = sum(p[1] for p in points) / n
    sxx = sum((p[0] - mx) ** 2 for p in points)
    slope = sum((p[0] - mx) * (p[1] - my) for p in points) / sxx
    return slope, my - slope * mx


def envelope(points):
    """A line through the least delayed reading of each bin — see the header."""
    slope, intercept = lsq(points)
    for _ in range(4):
        best = {}
        for t, position in points:
            b = int(t // ENVELOPE_BIN)
            residual = position - (slope * t + intercept)
            if b not in best or residual > best[b][2]:
                best[b] = (t, position, residual)
        slope, intercept = lsq([(t, p) for t, p, _ in best.values()])
    return slope, intercept


def held_out(points):
    """Reference from the even 60 s blocks; the odd blocks to score against it."""
    fit = [p for p in points if int(p[0] // BLOCK) % 2 == 0]
    test = [p for p in points if int(p[0] // BLOCK) % 2 == 1]
    return envelope(fit), test


class Clock:
    """`freerun::FreeRun`, with the anchor window as a parameter.

    `window=None` is the behaviour before the window existed: every reading is
    an anchor, which is what makes one-sided delivery error a bias rather than
    a scatter.
    """

    def __init__(self, fps, window):
        self.fps, self.window, self.state = fps, window, None
        self.raw, self.spans = [], []   # per-baseline rate, before clamping

    def _restart(self, position, at, rate, measurements):
        self.state = dict(pos=position, at=at, rate=rate, anchored=at,
                          rate_from=(position, at), meas=measurements, win=None)

    def extrapolate(self, now):
        return self.state["pos"] + (now - self.state["at"]) * self.state["rate"]

    def _measure_rate(self, position, at):
        state = self.state
        from_position, from_at = state["rate_from"]
        if (span := at - from_at) < RATE_BASELINE:
            return
        raw = (position - from_position) / span
        self.raw.append(raw)
        self.spans.append(span)
        lo, hi = self.fps * (1 - MAX_RATE_ERROR), self.fps * (1 + MAX_RATE_ERROR)
        state["rate"] += RATE_GAIN * (min(max(raw, lo), hi) - state["rate"])
        state["meas"] += 1
        state["rate_from"] = (position, at)

    def _fold_in(self, position, at):
        predicted = self.extrapolate(at)
        self._measure_rate(position, at)
        self.state["pos"] = predicted + SLEW_GAIN * (position - predicted)
        self.state["at"] = at

    def anchor(self, position, at):
        if self.state is None:
            return self._restart(position, at, float(self.fps), 0)
        state = self.state
        if at - state["anchored"] > HOLDOVER:
            return self._restart(position, at, state["rate"], state["meas"])
        if self.window is not None and state["win"] and at >= state["win"]["ends"]:
            closed, state["win"] = state["win"], None
            self._fold_in(closed["pos"], closed["at"])
        predicted = self.extrapolate(at)
        error = position - predicted
        if abs(error) > SNAP * state["rate"]:
            return self._restart(position, at, state["rate"], state["meas"])
        state["anchored"] = at
        if self.window is None:
            return self._fold_in(position, at)
        if (open_window := state["win"]) is None:
            state["win"] = dict(pos=position, at=at, error=error,
                                ends=at + self.window)
        elif error > open_window["error"]:
            open_window.update(pos=position, at=at, error=error)


def sd(xs):
    if len(xs) < 2:
        return 0.0
    mean = sum(xs) / len(xs)
    return math.sqrt(sum((x - mean) ** 2 for x in xs) / (len(xs) - 1))


def replay(points, fps, window, reference, test_times, warmup=180.0):
    slope, intercept = reference
    clock = Clock(fps, window)
    deviations, rate_errors, shown = [], [], []
    marked = -1e9
    for t, position in points:
        clock.anchor(position, t)
        if t < warmup or clock.state is None:
            continue
        if t in test_times:
            deviations.append(
                (clock.extrapolate(t) - (slope * t + intercept)) / fps * 1000.0
            )
        shown.append((clock.state["rate"] / fps - 1) * 1e6)
        if t - marked >= BLOCK:
            marked = t
            rate_errors.append((clock.state["rate"] - slope) / fps * 1e6)

    mean = sum(deviations) / len(deviations)
    ppm = [(x / fps - 1) * 1e6 for x in clock.raw]
    span = sum(clock.spans) / len(clock.spans) if clock.spans else 0.0
    settled = shown[len(shown) // 4:]
    rate_rms = math.sqrt(sum(e * e for e in rate_errors) / len(rate_errors))
    return dict(
        anchor_ms=sd(ppm) * 1e-6 * span / math.sqrt(2) * 1000.0,
        ppm_sd=sd(ppm), span=span, baselines=len(ppm),
        rms=math.sqrt(sum((d - mean) ** 2 for d in deviations) / len(deviations)),
        peak=max(abs(d - mean) for d in deviations),
        shown_sd=sd(settled), shown_lo=min(settled), shown_hi=max(settled),
        rate_rms=rate_rms,
        half_frame=(0.5 / fps) / (rate_rms * 1e-6) if rate_rms else float("inf"),
    )


def between(devices, fps, references):
    """The two boxes against each other, with the host clock differenced out."""
    names = sorted(devices)
    if len(names) != 2:
        return
    a, b = names
    print(f"\n=== {a} against {b} ===")
    if fps[a] != fps[b]:
        print("    different frame rates; not comparable")
        return
    f = fps[a]
    (slope_a, icept_a), (slope_b, icept_b) = references[a], references[b]
    relative = (slope_a - slope_b) / f * 1e6
    print(f"    relative rate {relative:+.3f} ppm  "
          f"({(slope_a / f - 1) * 1e6:+.2f} and {(slope_b / f - 1) * 1e6:+.2f} "
          f"against this host)")
    # Sampling both reference lines at one host instant. The sub-frame bias is
    # a firmware constant, so to the extent it is the same on both units it
    # cancels here too — which bounds this, since the two units' observed
    # floors differ by about the size of the offsets below.
    lo = max(devices[a][0][0], devices[b][0][0])
    hi = min(devices[a][-1][0], devices[b][-1][0])
    for fraction, label in ((0.0, "start"), (0.5, "middle"), (1.0, "end")):
        t = lo + fraction * (hi - lo)
        offset = ((slope_a * t + icept_a) - (slope_b * t + icept_b)) / f * 1000.0
        print(f"    offset at {label:>6}: {offset:+8.3f} ms  "
              f"({offset / (1000.0 / f):+.4f} frames)")
    parting = relative * 1e-6 * 3600 * 1000
    print(f"    parting at {parting:+.2f} ms/hour -> half a frame "
          f"({0.5 / f * 1000:.1f} ms) in "
          f"{abs(0.5 / f / ((slope_a - slope_b) / f)) / 3600:.1f} h"
          if slope_a != slope_b else "    identical rates")


def main(path):
    devices, fps = load(path)
    references = {}
    for name, points in sorted(devices.items()):
        f = fps[name]
        reference, test = held_out(points)
        references[name] = reference
        print(f"\n=== {name}  {f} fps  n={len(points)}  "
              f"span={points[-1][0] - points[0][0]:.0f}s  scored on n={len(test)} ===")
        print(f"    reference {(reference[0] / f - 1) * 1e6:+.2f} ppm against this host")
        print(f"    {'window':>7} {'anchor ms':>10} {'ppm sd':>7} {'base s':>7} "
              f"{'n':>4} {'rms ms':>7} {'peak ms':>8} "
              f"{'shown ppm (sd, range)':>24} {'rate err -> half frame':>25}")
        test_times = {t for t, _ in test}
        for window in WINDOWS:
            r = replay(points, f, window, reference, test_times)
            shown = f"{r['shown_sd']:.1f} [{r['shown_lo']:+.0f},{r['shown_hi']:+.0f}]"
            held = f"{r['rate_rms']:.1f} ppm -> {r['half_frame'] / 60:.0f} min"
            print(f"    {'none' if window is None else f'{window:g}s':>7} "
                  f"{r['anchor_ms']:10.3f} {r['ppm_sd']:7.1f} {r['span']:7.1f} "
                  f"{r['baselines']:4d} {r['rms']:7.3f} {r['peak']:8.3f} "
                  f"{shown:>24} {held:>25}")
    between(devices, fps, references)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[0] + "\n\nusage: freerun_replay.py CAPTURE.json")
    main(sys.argv[1])
