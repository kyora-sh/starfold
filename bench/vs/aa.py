#!/usr/bin/env python3
"""aa.py <run logs...>: a set's arms against its first arm, judged against the set's own A/A.

Each log is one set.sh run (its RUN lines and CALIB lines). Runs whose CALIB drifts more than --calib
(or whose CALIB is missing or not positive) are left out, as are rows whose steal is above --steal.
The first arm's twin (`<arm>~aa`) is the A/A: the spread S of a metric is max(|lo|, |hi|) of the
twin-minus-arm 95% bootstrap CI, and a pair's shift is real only if its CI excludes 0 and its median
difference is beyond S. Each metric uses the rows that have a finite value for it.
"""
import argparse, json, math, random, statistics as st

TWIN = '~aa'


def load(path):
    cal, rows = [], []
    for line in open(path):
        if line.startswith('CALIB'):
            try:
                cal.append(float(line.split()[1]))
            except (IndexError, ValueError):
                pass
        elif line.startswith('RUN '):
            rows.append(json.loads(line[4:]))
    return cal, rows


def drift(cal):
    """CALIB drift of a run, or None when it can't be told (missing, or not positive)."""
    if len(cal) != 2 or cal[0] <= 0 or cal[1] <= 0:
        return None
    return abs(cal[1] - cal[0]) / cal[0]


def values(rows, arm, metric):
    out = []
    for r in rows:
        v = r.get(metric)
        if r.get('arm') == arm and isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v):
            out.append(float(v))
    return out


def boot(x, y, n=10000):
    """Median difference x - y and its 95% bootstrap CI; None without samples on both sides."""
    if not x or not y:
        return None
    random.seed(1)
    d = st.median(x) - st.median(y)
    bs = sorted(st.median(random.choices(x, k=len(x))) - st.median(random.choices(y, k=len(y))) for _ in range(n))
    return d, bs[n // 40], bs[n - n // 40]


def analyze(logs, set_name='main', metrics=('i2pP50', 'i2pP95'), max_drift=0.15, max_steal=2.0):
    runs, notes = [], []
    for path in logs:
        cal, rows = load(path)
        d = drift(cal)
        keep = d is not None and d <= max_drift
        notes.append((path, cal, d, keep))
        if keep:
            kept = [r for r in rows if r.get('set') == set_name and (r.get('stealPct') or 0) <= max_steal]
            runs.append((path, kept))
    arms = list(dict.fromkeys(r['arm'] for _, R in runs for r in R if 'arm' in r))
    twin = next((x for x in arms if x.endswith(TWIN)), None)
    out = {'notes': notes, 'arms': arms, 'base': twin and twin[: -len(TWIN)], 'metrics': {}}
    if not twin:
        return out
    base = out['base']
    every = [r for _, R in runs for r in R]
    for m in metrics:
        aa = boot(values(every, twin, m), values(every, base, m))
        res = {'aa': aa, 'spread': aa and max(abs(aa[1]), abs(aa[2])), 'pairs': {}}
        for x in arms:
            if x in (base, twin):
                continue
            b = boot(values(every, x, m), values(every, base, m))
            if not b:
                continue
            per = []
            for _, R in runs:
                xv, bv = values(R, x, m), values(R, base, m)
                per.append(st.median(xv) - st.median(bv) if xv and bv else None)
            real = res['spread'] is not None and (b[1] > 0 or b[2] < 0) and abs(b[0]) > res['spread']
            res['pairs'][x] = (*b, per, real)
        out['metrics'][m] = res
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('logs', nargs='+')
    ap.add_argument('--set', default='main')
    ap.add_argument('--metrics', default='i2pP50,i2pP95')
    ap.add_argument('--calib', type=float, default=0.15)
    ap.add_argument('--steal', type=float, default=2.0)
    a = ap.parse_args()
    metrics = a.metrics.split(',')
    out = analyze(a.logs, a.set, metrics, a.calib, a.steal)
    for path, cal, d, keep in out['notes']:
        print(f"{path}: CALIB {' -> '.join(map(str, cal)) or 'missing'}"
              f"{f' ({d * 100:.1f}%)' if d is not None else ' (drift unknown)'}{'' if keep else ' LEFT OUT'}")
    if not out['base']:
        print('no A/A twin in these runs (AA=0?): nothing to judge against')
        return
    base = out['base']
    for m, res in out['metrics'].items():
        if not res['aa']:
            print(f"\n{m}: no A/A ({base}{TWIN} or {base} has no values): nothing to judge against")
            continue
        d, lo, hi = res['aa']
        print(f"\n{m}: A/A {base}{TWIN} - {base}: {d:+.1f} [{lo:+.1f}, {hi:+.1f}], spread S = {res['spread']:.1f}"
              f"{'  (the A/A CI excludes 0: the bench itself shifts by this much)' if lo > 0 or hi < 0 else ''}")
        for x, (d, lo, hi, per, real) in res['pairs'].items():
            p = ', '.join('-' if v is None else f'{v:+.1f}' for v in per)
            verdict = 'REAL ' + ('rise' if d > 0 else 'drop') if real else 'within the A/A'
            print(f"  {x} - {base}: {d:+.1f} [{lo:+.1f}, {hi:+.1f}]  per run {p}  -> {verdict}")


if __name__ == '__main__':
    main()
