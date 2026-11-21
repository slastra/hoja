# Used by grid-big.sh:  python3 watch-shifts.py PROBE SECONDS
#
# Poll the probe for `secs`; count cards that moved relative to the rest
# between consecutive readings. A scroll moves every card by the same step,
# so only a card whose step differs from the most common one, or whose x or
# height changed, is a shift.
import json, sys, time, collections
probe, secs = sys.argv[1], float(sys.argv[2])
def read():
    try:
        d = json.load(open(probe))
        return {c[0]: c[1:5] for c in d["panes"][0]["cards"]}
    except Exception:
        return None
prev = read(); moved = 0; end = time.time() + secs
while time.time() < end:
    time.sleep(0.05)
    cur = read()
    if not cur or not prev:
        prev = cur; continue
    common = [k for k in cur if k in prev]
    if common:
        steps = collections.Counter(cur[k][1] - prev[k][1] for k in common)
        usual = steps.most_common(1)[0][0]
        moved += sum(1 for k in common
                     if cur[k][1] - prev[k][1] != usual
                     or cur[k][0] != prev[k][0] or cur[k][3] != prev[k][3])
    prev = cur
print(moved)
