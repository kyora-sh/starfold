#!/usr/bin/env python3
"""aa.py <run logs...>: a set's arms against its first arm, judged against the set's own A/A.

Each log is one set.sh run (its RUN lines and CALIB lines). Runs whose CALIB drifts more than --calib
(or whose CALIB is missing or not positive) are left out, as are rows whose steal is above --steal.
A row with no steal reading can't be judged by that rule: it stays in and is reported as steal unknown.
The first arm's twin (`<arm>~aa`) is the A/A: the spread S of a metric is max(|lo|, |hi|) of the
twin-minus-arm 95% bootstrap CI, and a pair's shift is real only if its CI excludes 0 and its median
difference is beyond S. Each metric uses the rows that have a finite value for it.
--usable 500,1000 counts, per arm, the quality rows whose settled<ms>Psnr is at least --db: a failed row
stays in the arm's denominator as not usable. Each arm against the first: two-sided Fisher exact, and its
share's difference is beyond the A/A only if it exceeds |twin share - first arm's share|.
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


def steal(row):
    """A row's steal share, or None when it has no usable reading."""
    v = row.get('stealPct')
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v) else None


def boot(x, y, n=10000):
    """Median difference x - y and its 95% bootstrap CI; None without samples on both sides."""
    if not x or not y:
        return None
    random.seed(1)
    d = st.median(x) - st.median(y)
    bs = sorted(st.median(random.choices(x, k=len(x))) - st.median(random.choices(y, k=len(y))) for _ in range(n))
    return d, bs[n // 40], bs[n - n // 40]


def kept(logs, set_name, max_drift, max_steal):
    """The runs whose CALIB passes, each with its rows of the set that pass the steal rule; the CALIB notes; rows of unknown steal per run."""
    runs, notes, unknown = [], [], {}
    for path in logs:
        cal, rows = load(path)
        d = drift(cal)
        keep = d is not None and d <= max_drift
        notes.append((path, cal, d, keep))
        if keep:
            k = [r for r in rows if r.get('set') == set_name and (steal(r) is None or steal(r) <= max_steal)]
            runs.append((path, k))
            unknown[path] = sum(steal(r) is None and not r.get('failed') for r in k)
    arms = list(dict.fromkeys(r['arm'] for _, R in runs for r in R if 'arm' in r))
    twin = next((x for x in arms if x.endswith(TWIN)), None)
    return runs, {'notes': notes, 'stealUnknown': unknown, 'arms': arms, 'base': twin and twin[: -len(TWIN)]}


def fisher(a, b, c, d):
    """Two-sided Fisher exact p of the 2x2 table [[a, b], [c, d]]."""
    n1, n2, k = a + b, c + d, a + c
    p = lambda x: math.comb(n1, x) * math.comb(n2, k - x) / math.comb(n1 + n2, k)
    seen = p(a)
    return min(1.0, sum(p(x) for x in range(max(0, k - n2), min(k, n1) + 1) if p(x) <= seen * (1 + 1e-9)))


def usable(logs, points=(500, 1000), db=25.0, max_drift=0.15, max_steal=2.0):
    """Per settle point, each arm's usable rows out of its rows (failed ones included), and each arm against the first."""
    runs, out = kept(logs, 'quality', max_drift, max_steal)
    out['points'] = {}
    every = [r for _, R in runs for r in R]
    for t in points:
        counts = {}
        for x in out['arms']:
            R = [r for r in every if r.get('arm') == x]
            ok = sum(isinstance(r.get(f'settled{t}Psnr'), (int, float)) and r[f'settled{t}Psnr'] >= db for r in R)
            counts[x] = (ok, len(R), sum(bool(r.get('failed')) for r in R))
        res = {'counts': counts, 'pairs': {}}
        base = out['base']
        if base and base in counts and base + TWIN in counts:
            share = lambda x: counts[x][0] / counts[x][1] if counts[x][1] else math.nan
            res['aa'] = abs(share(base + TWIN) - share(base))
            for x in out['arms']:
                if x in (base, base + TWIN):
                    continue
                (k1, n1, _), (k0, n0, _) = counts[x], counts[base]
                p = fisher(k1, n1 - k1, k0, n0 - k0)
                d = share(x) - share(base)
                res['pairs'][x] = (d, p, p < 0.05 and abs(d) > res['aa'])
        out['points'][t] = res
    return out


def analyze(logs, set_name='main', metrics=('i2pP50', 'i2pP95'), max_drift=0.15, max_steal=2.0):
    runs, out = kept(logs, set_name, max_drift, max_steal)
    out['metrics'] = {}
    arms, twin = out['arms'], out['base'] and out['base'] + TWIN
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
    ap.add_argument('--usable', help='settle points (ms) to count usable quality rows at, e.g. 500,1000')
    ap.add_argument('--db', type=float, default=25.0)
    a = ap.parse_args()
    if a.usable:
        out = usable(a.logs, [int(t) for t in a.usable.split(',')], a.db, a.calib, a.steal)
    else:
        out = analyze(a.logs, a.set, a.metrics.split(','), a.calib, a.steal)
    for path, cal, d, keep in out['notes']:
        print(f"{path}: CALIB {' -> '.join(map(str, cal)) or 'missing'}"
              f"{f' ({d * 100:.1f}%)' if d is not None else ' (drift unknown)'}{'' if keep else ' LEFT OUT'}")
        if out['stealUnknown'].get(path):
            print(f"  {out['stealUnknown'][path]} rows steal unknown (no reading): kept, not checked against --steal")
    if not out['base']:
        print('no A/A twin in these runs (AA=0?): nothing to judge against')
        return
    base = out['base']
    for t, res in out.get('points', {}).items():
        print(f"\nusable at +{t} ms (settled{t}Psnr >= {a.db:g} dB; failed rows count as not usable):")
        for x, (k, n, failed) in res['counts'].items():
            print(f"  {x}: {k}/{n}{f' ({failed} failed)' if failed else ''}")
        if 'aa' not in res:
            print(f"  no A/A ({base}{TWIN} or {base} has no rows): nothing to judge against")
            continue
        print(f"  A/A |{base}{TWIN} - {base}| share: {res['aa']:.2f}")
        for x, (d, p, real) in res['pairs'].items():
            print(f"  {x} - {base}: {d:+.2f} share, Fisher p {p:.3f}  -> {'REAL ' + ('rise' if d > 0 else 'drop') if real else 'within the A/A'}")
    for m, res in out.get('metrics', {}).items():
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
