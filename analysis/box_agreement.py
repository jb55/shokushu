#!/usr/bin/env python3
"""How far apart two Tentacles' clocks are, measured without connecting to either.

    cargo run -q --features scan,cli --bin shokushu-ble -- --json --seconds 120 > before.jsonl
    python3 analysis/box_agreement.py before.jsonl

Why this can work at all, when nothing that only listens can measure one box's
offset. For each reading, `host_micros - timecode` is that box's delivery delay
minus its own clock offset:

    b_i = d_i - theta_i

Neither term is knowable alone. But every box in one capture reaches the same
host over the same kind of path, so their delivery *floors* are the same
quantity, and differencing two boxes cancels it along with the host clock:

    min(b_i) - min(b_j) = theta_j - theta_i

which is exactly how far apart the two boxes are. The host clock being free-
running and unknown does not matter; it appears in both and subtracts out.

That the floors are equal is the one assumption, and it is the reason this uses
a minimum rather than a mean. A mean would carry each box's whole delay
distribution, which differs with signal strength and how often it is heard from.
A floor is the best case of a path, and the best case of two identical radios at
the same distance is the same. `PROTOCOL.md` measures the free-running
advertisement floor at 0.35 ms, so this resolves box-to-box differences to
somewhat better than a millisecond — a fortieth of a frame at 24 fps.

**Matched counts.** A minimum over fewer samples sits higher for that reason
alone, so a box heard from half as often would look slower rather than later.
Every figure here is computed over the same number of readings per box, taken
from the front of the capture. This is the check that caught a wrong answer once
already; see `PROTOCOL.md`.

# The question this exists to settle

`PROTOCOL.md` records, under Open questions, that two boxes have been seen out
of sync after sessions of GATT work, and lists three candidate mechanisms. The
experiment is cheap: measure the boxes against each other passively, connect to
exactly one of them, measure again. If the box that was connected to has moved
relative to the others, a bare connection re-jams it. If nothing moved, it does
not — and the suspicion moves to the recovery from a wedged box, or to ordinary
drift.

Run it as two captures with one connection between them:

    ... --json --seconds 120 > before.jsonl
    ... --jam --name ricki --seconds 120        # the one connection
    ... --json --seconds 120 > after.jsonl
    python3 analysis/box_agreement.py before.jsonl after.jsonl

Drift is the thing to be careful of here, not noise: two boxes 8.6 ppm apart
move 0.09 ms in the ten seconds between captures but 31 ms in an hour, so the
two captures want to be close together and the elapsed time is reported so the
drift can be subtracted by eye.
"""

import json
import sys
from collections import defaultdict

# Half a frame at 24 fps, which is what it takes to change a displayed frame
# number and so the scale at which "out of sync" means anything to a user.
HALF_FRAME_24 = 1000.0 / 24 / 2


def read(path):
    """One capture as {device: [(host_seconds, timecode_seconds), ...]}."""
    rows = defaultdict(list)
    for n, line in enumerate(open(path), 1):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            print(f"{path}:{n}: not JSON, skipped", file=sys.stderr)
            continue
        # A capture from an older build has no host stamp, and without one
        # there is nothing to measure against. Say so rather than guessing.
        if r.get("host_micros") is None or r.get("fps") is None:
            continue
        tc = (
            r["hours"] * 3600
            + r["minutes"] * 60
            + r["seconds"]
            + r["frames"] / r["fps"]
            + r["subframe_micros"] / 1e6
        )
        rows[r["device"]].append((r["host_micros"] / 1e6, tc))
    return rows


def floors(rows, matched):
    """min(host - timecode) per box, over the same count for each."""
    out = {}
    for device, readings in sorted(rows.items()):
        if len(readings) < matched:
            continue
        out[device] = min(host - tc for host, tc in readings[:matched])
    return out


def converged(readings, matched):
    """The floor over growing prefixes, so a floor still falling is visible.

    A minimum is a biased estimator and creeps down as samples accumulate. If
    the last step is still moving by an appreciable fraction of the differences
    being reported, the capture is too short and the differences are noise.
    """
    steps = []
    for cut in range(max(matched // 5, 1), matched + 1, max(matched // 5, 1)):
        steps.append((cut, min(host - tc for host, tc in readings[:cut])))
    return steps


def report(path, rows):
    if len(rows) < 2:
        print(f"{path}: {len(rows)} box(es) — need two to compare\n")
        return None
    matched = min(len(v) for v in rows.values())
    got = floors(rows, matched)
    span = max(max(h for h, _ in v) for v in rows.values())

    print(f"{path}: {len(got)} boxes, {span:.0f} s, matched at n={matched}")
    for device, readings in sorted(rows.items()):
        steps = converged(readings, matched)
        walk = "  ".join(f"n={n}:{(f - got[device]) * 1e3:+.2f}" for n, f in steps)
        print(f"  {device:<10} {len(readings):5} readings   floor walk (ms) {walk}")

    print()
    reference = min(got.values())
    for device, floor in sorted(got.items(), key=lambda kv: kv[1]):
        ms = (floor - reference) * 1e3
        print(f"  {device:<10} {ms:+8.3f} ms   {ms / HALF_FRAME_24 * 0.5:+6.3f} frames @24")
    print()
    return got


def compare(before, after):
    """What moved between two captures, per box and pairwise."""
    shared = sorted(set(before) & set(after))
    if len(shared) < 2:
        print("fewer than two boxes in both captures — nothing to compare")
        return

    print("=== what moved between the two captures")
    print()
    print("  A single box's floor moving says nothing: the host clock is free-")
    print("  running too, so a change common to every box is this computer's.")
    print("  Only a box moving *relative to the others* is that box moving.")
    print()
    common = min(after[d] - before[d] for d in shared)
    for device in shared:
        moved = (after[device] - before[device] - common) * 1e3
        mark = "   <-- moved" if abs(moved) > HALF_FRAME_24 else ""
        print(
            f"  {device:<10} {moved:+8.3f} ms   "
            f"{moved / HALF_FRAME_24 * 0.5:+6.3f} frames @24{mark}"
        )
    print()
    worst = max(
        abs(after[d] - before[d] - common) for d in shared
    ) * 1e3
    if worst > HALF_FRAME_24:
        print(f"  Largest relative move {worst:.3f} ms is past half a frame at 24 fps,")
        print("  which is what it takes to change a displayed frame number.")
    else:
        print(f"  Largest relative move {worst:.3f} ms, under the {HALF_FRAME_24:.1f} ms")
        print("  half-frame at 24 fps. Nothing here desynced the boxes by an amount")
        print("  anyone would see — which is not the same as nothing having moved.")


def main():
    paths = sys.argv[1:]
    if not paths:
        print(__doc__.strip().split("\n\n")[1], file=sys.stderr)
        raise SystemExit(2)
    got = [report(p, read(p)) for p in paths]
    if len(paths) == 2 and all(g is not None for g in got):
        compare(got[0], got[1])


if __name__ == "__main__":
    main()
