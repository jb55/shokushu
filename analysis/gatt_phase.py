#!/usr/bin/env python3
"""Bounds the offset between a Tentacle's clock and this host's, from GATT round trips.

The `freerun` module docs say a clock built from advertisements sits at an
unknown fixed offset from the device's, and that how big that offset is, is
itself unmeasured. This measures it — or rather, bounds it, which is as much as
a Bluetooth link will give up.

    cargo run --release --features scan,cli --bin shokushu-gatt -- \\
        --seconds 300 --reconnect --phase capture.jsonl
    python3 analysis/gatt_phase.py capture.jsonl

No dependencies. Fast: the arithmetic is a few minima.

# Why a read and not an advertisement

An advertisement is one-way, so nothing that only listens can take apart the
fixed quantity between the two clocks: a transmit-path constant, a flight time
and a stack delay all look the same from the receiving end. An ATT read is a
round trip. Stamp the host clock either side of one and the timecode in the
response stands in for the device's own two stamps:

    t0 ---- Read Request ---> | device stamps T | ---- Read Response ---> t1

Write `a = T - t0` and `b = t1 - T`, and let `theta` be the thing we want: the
device's clock minus this host's, at the same instant. Then

    a = d_out + theta        b = d_ret - theta

for the two one-way delays `d_out` and `d_ret`. Neither is knowable alone. But
both are elapsed times, so both are at least zero, and that alone gives

    theta <= a      and      theta >= -b

for *every single sample*. So `min(a)` bounds the offset above and `-min(b)`
bounds it below.

# What that costs, and what it does not

**It does not assume the two legs are equal.** They are conspicuously not: a
request handed to the controller waits for the next connection anchor point and
a response, already at the device, does not. Halving the round trip — NTP's
usual move — would put the answer at the midpoint of the bracket below and
quietly take that asymmetry on as bias, of the same order as the 3.6 ms the
whole exercise is trying to resolve. The bracket is the honest form. Its
midpoint is printed, and it is a midpoint and not a measurement.

**It does assume the device's stamp falls between the two host stamps.** If the
stamp were from before the request went out, `d_out` would be negative and the
upper bound would be worth nothing. That is not hypothetical: a capture taken
while subscribed breaks it every time, because on macOS a Read Response and a
notification arrive through the same CoreBluetooth callback and a pending read
is resolved by whichever lands first. The value is real, the timing is not, and
the give-away is a round trip shorter than the 30 ms connection interval, which
a real one cannot be. `validity` counts those and says so; capture with
`--no-subscribe`.

Given a clean capture, the check that remains is internal: a bracket has to
come out positive. `min(a) + min(b) = min(d_out) + min(d_ret) >= 0` is
guaranteed if the stamp really is between the two, so a negative width is proof
that it is not.

**The bound includes everything fixed.** The ~3.6 ms origin bias in the
microsecond counter that PROTOCOL.md measures is part of `theta` here, on
purpose: what a caller wants to know is how far the timecode a box reports sits
from this host's clock, and a bias in the counter's origin is as much a part of
that as a flight time is.

# Drift, and why the report is per block

The two crystals run at different rates, so `theta` is not constant: at the 5
ppm `--drift` reports it moves 1.5 ms over a 300 s capture, which is half the
width of the bracket being measured. Taking one minimum over a whole capture
would therefore report a bracket most of whose width is drift.

So the bracket is computed over short blocks, inside which drift is negligible
(0.15 ms over 30 s), and the blocks are reported separately. The trend across
their midpoints is then itself a drift measurement, arrived at from round trips
rather than from one-way anchors — an independent check on the `--drift`
column, from data that column never sees. `--block` sets the length.

# The columns

- `bracket` — `[-min(b), min(a)]`, the interval the offset provably lies in,
  reported about the run's own mean because the absolute value is a device
  time-of-day minus a host monotonic count and means nothing on its own.
- `width` — `min(a) + min(b)`, which is `min(d_out) + min(d_ret)`: the two path
  floors added. **This is the result.** It is *narrower* than the shortest
  round trip, because the two minima are achieved by different samples — one
  where the request waited least, one where the response did.
- `n` — round trips in the block. The minima are biased estimators of the
  floors and creep down as this grows, so a block with few samples reports a
  bracket that is too wide rather than too narrow. That is the safe direction,
  and `converge` shows how much of it is left.

# What is not measured here

The offset on the **advertisement** path, which is the one `freerun` cares
about. A GATT round trip measures the connection path. The `paths` section
differences the floors of the streams a capture holds — reads, notifications
and, with `--scan`, advertisements — and `theta` cancels out of that difference,
leaving the delivery cost of one path against another. That is the bridge, and
it is only as good as the advertisement count, which a connected scan makes
small.
"""

import collections
import json
import math
import sys

# Default block length in seconds. Short enough that drift inside one is well
# under the bracket it is measuring, long enough to hold a few connections'
# worth of round trips.
BLOCK = 30.0

# The connection interval, in seconds. PROTOCOL.md measures it at 30 ms from
# the notification gap histogram — 3,076 of 3,091 gaps within 4 ms of a
# multiple of it. Used only to recognise a round trip that is too short to be
# one; `--interval=` overrides it in ms if a box ever negotiates another.
INTERVAL = 0.030


def load(path):
    """Every record in the capture, as dicts, with device time resolved.

    `device` is seconds since the device's midnight, the microsecond trailer
    included. `host` is seconds since the run started, off an `Instant`, so it
    is an interval and not a time of day. The difference between them is
    therefore a huge number with no meaning of its own — only differences of
    differences mean anything, which is why every figure below is reported
    about its own mean.
    """
    rows = []
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        row = json.loads(line)
        if row["kind"] == "session":
            rows.append(row)
            continue
        if row.get("fps") is None:
            # A payload that did not decode. Recorded by the capture on
            # purpose; skipped here, and counted at the end so a capture that
            # was mostly undecodable says so instead of looking clean.
            rows.append(row)
            continue
        frames = (row["hours"] * 3600 + row["minutes"] * 60 + row["seconds"]) * row["fps"]
        row["device"] = (frames + row["frames"]) / row["fps"] + row["subframe_micros"] / 1e6
        if row["kind"] == "read":
            row["t0"] = row["t0_micros"] / 1e6
            row["t1"] = row["t1_micros"] / 1e6
            row["host"] = row["t1"]
            row["a"] = row["device"] - row["t0"]
            row["b"] = row["t1"] - row["device"]
            row["rtt"] = row["t1"] - row["t0"]
        else:
            row["host"] = row["at_micros"] / 1e6
            # A one-way stream has no departure stamp, so all it can give is
            # the `b` half: an arrival minus a device stamp, which bounds the
            # offset from below and says nothing about it from above.
            row["b"] = row["host"] - row["device"]
        rows.append(row)
    return rows


def blocks(rows, length):
    """Group by `host` into blocks of `length` seconds."""
    out = collections.defaultdict(list)
    for row in rows:
        out[int(row["host"] // length)].append(row)
    return sorted(out.items())


def fit(points):
    """Theil-Sen slope and intercept of (x, y) pairs, or None if degenerate.

    The median of all pairwise slopes, not least squares. These are block
    bracket midpoints — few of them, and one bad block moves its midpoint by
    tens of milliseconds where the honest ones sit within a fraction of one.
    Least squares hands such a block most of the fit: on a 314 s capture a
    single bad block turned +8.8 ppm into -23.3 ppm at 3.8 ms rms, and since
    the slope is then used to de-trend everything pooled, the wrong slope
    quietly turned the bracket negative rather than failing.

    Theil-Sen ignores it. It tolerates up to 29% of the points being arbitrary,
    needs no threshold to be chosen, and on clean input agrees with least
    squares to well inside what any of this resolves.
    """
    n = len(points)
    if n < 2:
        return None
    slopes = [
        (points[j][1] - points[i][1]) / (points[j][0] - points[i][0])
        for i in range(n)
        for j in range(i + 1, n)
        if points[j][0] != points[i][0]
    ]
    if not slopes:
        return None
    slopes.sort()
    slope = slopes[len(slopes) // 2]
    # The intercept that puts the line through the median residual, which is
    # the matching robust choice — a mean here would let the outlier back in.
    offsets = sorted(y - slope * x for x, y in points)
    return slope, offsets[len(offsets) // 2]


def contradictory(reads, length):
    """Block indices whose bracket came out negative — see `report_brackets`."""
    out = []
    for index, rows in blocks(reads, length):
        if min(r["a"] for r in rows) + min(r["b"] for r in rows) < 0:
            out.append(index)
    return set(out)


def report_brackets(reads, length):
    """The bracket per block, and the drift its midpoints imply."""
    print(f"# The bracket, per {length:g} s block")
    print()
    print("  Every sample gives theta <= a and theta >= -b, so the block's")
    print("  tightest pair of those bounds it. Offsets are about the run's mean,")
    print("  because the absolute number is a device time-of-day minus a host")
    print("  monotonic count.")
    print()
    grouped = blocks(reads, length)
    if not grouped:
        print("  no round trips in the capture")
        return None
    # One reference for every block, so the columns are comparable.
    base = sum(r["a"] for r in reads) / len(reads)
    print("   block      n     lower      upper     width    min rtt")
    print("       s              ms         ms        ms         ms")
    mids = []
    widths = []
    for index, rows in grouped:
        lower = -min(r["b"] for r in rows)
        upper = min(r["a"] for r in rows)
        rtt = min(r["rtt"] for r in rows)
        at = index * length + length / 2
        width = upper - lower
        # A negative width says `min(a) + min(b) < 0`, and since both are sums
        # of a delay and the offset, that is only possible if some delay came
        # out below zero — a stamp from before the request that "returned" it.
        # Such a block is not a tighter bound, it is a broken one, and its
        # midpoint would drag the drift fit too.
        flag = "  contradictory" if width < 0 else ""
        if width >= 0:
            mids.append((at, (lower + upper) / 2))
            widths.append(width)
        print(
            f"  {index * length:6.0f} {len(rows):6d} {(lower - base) * 1e3:9.3f} "
            f"{(upper - base) * 1e3:10.3f} {(upper - lower) * 1e3:9.3f} {rtt * 1e3:10.3f}"
            f"{flag}"
        )
    print()
    bad = len(grouped) - len(widths)
    if bad:
        print(f"  {bad} of {len(grouped)} blocks came out with a negative width, which is")
        print("  impossible for a real round trip and means at least one sample in each")
        print("  was not one. They are excluded from everything below and their reads")
        print("  are dropped, since a pooled minimum would inherit the same sample.")
        print()
    if not widths:
        print("  no usable block: nothing here bounds anything.")
        print()
        return None
    print(
        f"  tightest usable block bracket {min(widths) * 1e3:.3f} ms,"
        f" median {sorted(widths)[len(widths) // 2] * 1e3:.3f} ms"
    )
    print()
    line = fit(mids)
    if line and len(mids) >= 3:
        slope, intercept = line
        residual = math.sqrt(
            sum((mid - (slope * at + intercept)) ** 2 for at, mid in mids) / len(mids)
        )
        print(f"  the midpoints move at {slope * 1e6:+.1f} ppm over the capture,")
        print(f"  scattering {residual * 1e3:.3f} ms rms about that line. That is a drift")
        print("  figure from round trips, and `shokushu-ble --drift` measures the same")
        print("  quantity from one-way anchors: if the two disagree, one of them is wrong.")
        print()
    elif line:
        # Two points define a line and say nothing about whether it is one.
        print(f"  {len(mids)} blocks is too few to call a drift rate; use a longer")
        print("  capture or a shorter --block if that is what you came for.")
        print()
    return line


def report_validity(reads, interval):
    """Whether these round trips are round trips.

    A read taken while subscribed is resolved by the notification stream rather
    than by a Read Response — see the module docstring — and the resulting
    sample has a real timecode in it and a fictitious round trip around it.
    Since a genuine round trip cannot be shorter than the connection interval
    the device sets, counting the ones that are separates the two cases
    cleanly. Nothing else here is worth reading if this section complains.
    """
    print("# Are these round trips")
    print()
    short = [r for r in reads if r["rtt"] < interval]
    fastest = min(r["rtt"] for r in reads)
    print(f"  shortest round trip {fastest * 1e3:.3f} ms")
    print(
        f"  {len(short)} of {len(reads)} ({100 * len(short) / len(reads):.1f}%) came back in"
        f" under {interval * 1e3:.0f} ms,"
    )
    print("  which is one connection interval and the least a round trip can take.")
    print()
    if len(short) > 0.02 * len(reads):
        print("  *** These are not round trips. A read taken while subscribed is")
        print("  *** resolved by the notification stream, not by a Read Response, so")
        print("  *** its timing is fiction. Re-capture with --no-subscribe. The")
        print("  *** bracket below is meaningless and is printed only to show it.")
    else:
        print("  Consistent with every sample being a real round trip.")
    print()


def detrend(rows, slope):
    """`a` and `b` referred back to the start of the capture.

    `theta` is not constant — the two crystals differ — so pooling raw minima
    over a long capture does not bound anything. Worse, it does not fail
    safely: `a = d_out + theta` rises with a positive drift while
    `b = d_ret - theta` falls, so `min(a)` is taken from early in the run and
    `min(b)` from late, and the two bounds are on *different* values of theta.
    The interval they enclose is spuriously narrow and is not a bound at all.

    Taking the fitted rate out first fixes it: with `theta(t) = theta0 + s*t`,
    `a - s*t = d_out + theta0` and `b + s*t = d_ret - theta0`, so both refer to
    the same `theta0` and pooling is legitimate again.
    """
    for row in rows:
        if "a" in row:
            row["a_flat"] = row["a"] - slope * row["host"]
        if "b" in row:
            row["b_flat"] = row["b"] + slope * row["host"]


def report_converge(reads):
    """Whether the run was long enough for the minima to have found the floors.

    A minimum over n samples is a biased estimator of a floor and creeps down
    as n grows, so a bracket from few samples is too *wide*. That is the safe
    direction — it over-reports the uncertainty rather than under-reporting it
    — but a width still falling steeply at the end of the capture is a width
    that has not been measured yet, only bounded.
    """
    print("# Does it converge")
    print()
    print("  Drift is taken out first (see `detrend`), so these pool honestly.")
    print("  A minimum creeps downwards as samples accumulate, so a short run")
    print("  reports a bracket that is too wide — safe, but only worth quoting")
    print("  once it has stopped moving.")
    print()
    ordered = sorted(reads, key=lambda r: r["host"])
    print("       n     width     min rtt")
    print("            ms          ms")
    step = max(1, len(ordered) // 8)
    width = None
    for cut in range(step, len(ordered) + 1, step):
        window = ordered[:cut]
        width = min(r["a_flat"] for r in window) + min(r["b_flat"] for r in window)
        rtt = min(r["rtt"] for r in window)
        print(f"  {cut:6d} {width * 1e3:9.3f} {rtt * 1e3:11.3f}")
    print()
    return width


def report_paths(rows, reads):
    """What the round trip says about the one-way paths — the point of all this.

    A round trip bounds `theta`. What a caller actually wants to know is
    something else: when a reading arrives off a *one-way* stream and gets
    treated as current — which is exactly what `freerun` does with an
    advertisement — how far behind the device's real clock does that put you?

    That quantity is the reading's staleness on arrival, `d = b + theta` for
    that stream's `b`. Both parts are now in hand: `b` is measured directly,
    and `theta` is bracketed by the round trips. So

        d >= min(b) - min(b_read)        and        d <= min(b) + min(a_read)

    for the least delayed reading the stream produced. The lower bound is the
    interesting one — it is what no amount of picking the best advertisement
    can beat.
    """
    print("# What a one-way reading costs")
    print()
    print("  A round trip bounds theta; a one-way stream then inherits that bound.")
    print("  For each stream below, `staleness` is how far behind the device's own")
    print("  clock its *least delayed* reading was when it landed — which is the")
    print("  error left in a clock that anchors on the best reading it sees, and so")
    print("  the floor under `freerun`'s accuracy on that path.")
    print()
    streams = collections.defaultdict(list)
    for row in rows:
        if "b_flat" in row:
            streams[row["kind"]].append(row)
    # theta's upper bound can only come from a round trip: `a` needs a
    # departure stamp and a one-way stream has none. The lower bound can come
    # from any stream, and should come from whichever gives the tightest one —
    # if advertisements reach the host sooner after being stamped than read
    # responses do, they constrain theta better than the round trips do on that
    # side, and using the reads' floor there would both loosen the bracket and
    # report a negative staleness, which no delivery delay can be.
    upper_read = min(r["a_flat"] for r in reads)
    lower_read = min(min(r["b_flat"] for r in rows_of) for rows_of in streams.values())
    fps = next((r["fps"] for r in reads if r.get("fps")), None)
    frame = 1.0 / fps if fps else None
    print("   stream        n    staleness of the least delayed    of a frame")
    for kind, rows_of in sorted(streams.items()):
        floor = min(r["b_flat"] for r in rows_of)
        low = floor - lower_read
        high = floor + upper_read
        frames = f"{low / frame:.2f}-{high / frame:.2f}" if frame else "—"
        print(
            f"  {kind:>8} {len(rows_of):8d}    {low * 1e3:9.3f} to {high * 1e3:8.3f} ms"
            f"    {frames:>11}"
        )
    print()
    print("  Low ends are against whichever stream reached the host soonest after")
    print("  being stamped, so that stream reads zero by construction and the")
    print("  others are measured against it. A high end is that stream's floor plus")
    print("  the round trips' bracket, which is the only thing that bounds theta")
    print("  from above.")
    print()
    if "advert" not in streams:
        print("  No advertisements here, so the path `freerun` actually uses is not")
        print("  measured. Re-run with --scan --no-subscribe, and expect them to be")
        print("  thin on the ground: a connected box advertises far less.")
        print()
        return
    adverts = streams["advert"]
    # Does suppression move the floor, or only how often it gets sampled?
    # A box advertises about a third as often while a central holds a link, and
    # the constant is calibrated during a connection but applied to a clock
    # that is never in one. Compared at matched sample counts, because a
    # minimum over fewer samples sits higher for that reason alone and would
    # otherwise look like a slower path.
    inside = sorted((r for r in adverts if r.get("connected")), key=lambda r: r["host"])
    between = sorted((r for r in adverts if r.get("connected") is False), key=lambda r: r["host"])
    print("  Connected against free-running, at matched n:")
    print()
    if not between:
        print("    no advertisements from between connections in this capture.")
        print("    Older captures cannot answer this — the tool only logged them")
        print("    during a session. Re-capture to compare.")
    else:
        n = min(len(inside), len(between))
        floor_in = min(r["b_flat"] for r in inside[:n]) - lower_read
        floor_out = min(r["b_flat"] for r in between[:n]) - lower_read
        print(f"    connected  n={n:4d}   floor {floor_in * 1e3:8.3f} ms")
        print(f"    between    n={n:4d}   floor {floor_out * 1e3:8.3f} ms")
        print(f"    difference             {(floor_out - floor_in) * 1e3:+8.3f} ms")
        print()
        print("    A difference near zero means suppression changes how often the")
        print("    floor is sampled and not where it is, so a constant calibrated")
        print("    inside a connection may be applied to a passive clock. A large")
        print("    one means it may not, and the calibration has to come from the")
        print("    free-running stream instead.")
    print()
    print("  Whether the advertisement figure has settled:")
    print()
    ordered = sorted(adverts, key=lambda r: r["host"])
    step = max(1, len(ordered) // 6)
    for cut in range(step, len(ordered) + 1, step):
        floor = min(r["b_flat"] for r in ordered[:cut])
        print(f"    n={cut:5d}   floor {(floor - lower_read) * 1e3:8.3f} ms")
    print()
    print("  Still falling at the last row means the advertisement floor has not")
    print("  been found and the true staleness is *lower* than reported — the")
    print("  opposite of the safe direction the bracket errs in, so say so rather")
    print("  than quoting the number flat. A connected box advertises rarely, which")
    print("  is why this column is the thin one.")
    print()


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    length = BLOCK
    interval = INTERVAL
    for arg in sys.argv[1:]:
        if arg.startswith("--block="):
            length = float(arg.split("=", 1)[1])
        if arg.startswith("--interval="):
            interval = float(arg.split("=", 1)[1]) / 1e3
    if not args:
        print(__doc__)
        return 1
    rows = load(args[0])
    reads = [r for r in rows if r["kind"] == "read" and "a" in r]
    undecoded = sum(1 for r in rows if r["kind"] != "session" and "device" not in r)
    sessions = sum(1 for r in rows if r["kind"] == "session" and r["event"] == "connected")
    span = max((r["host"] for r in rows if "host" in r), default=0.0)
    print()
    print(f"{args[0]}: {len(reads)} round trips over {span:.0f} s in {sessions} connection(s)")
    if undecoded:
        print(f"  {undecoded} payload(s) did not decode and are not counted")
    print()
    if not reads:
        print("no round trips: was --phase given to a run that could read?")
        return 1
    # A timecode that wraps midnight — or a box re-jammed mid-capture — drops
    # the device clock by a whole day or jumps it, and since everything here is
    # a minimum, one such sample silently becomes the answer. Cheap to notice
    # and impossible to spot in the output afterwards.
    ordered = sorted(reads, key=lambda r: r["host"])
    for before, after in zip(ordered, ordered[1:]):
        step = (after["device"] - before["device"]) - (after["host"] - before["host"])
        if abs(step) > 1.0:
            print(f"the device clock jumped {step:+.1f} s at {after['host']:.1f} s in.")
            print("A midnight wrap or a re-jam. Every figure here is a minimum, so")
            print("one such step becomes the answer; split the capture and re-run.")
            return 1
    report_validity(reads, interval)
    drift = report_brackets(reads, length)
    # Reads from a contradictory block are dropped rather than merely flagged:
    # every figure past this point is a minimum, so one impossible sample would
    # become the answer wherever it was pooled. Advertisements in the same
    # window are kept — the contradiction is a property of the round trips.
    bad = contradictory(reads, length)
    if bad:
        rows = [r for r in rows if r["kind"] != "read" or int(r["host"] // length) not in bad]
        reads = [r for r in reads if int(r["host"] // length) not in bad]
        if not reads:
            print("every block was contradictory; nothing to report.")
            return 1
    # Everything pooled from here on is referred back to the start of the
    # capture, because a bracket pooled across drift is not a bracket.
    detrend(rows, drift[0] if drift else 0.0)
    report_converge(reads)
    report_paths(rows, reads)
    return 0


if __name__ == "__main__":
    sys.exit(main())
