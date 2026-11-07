#!/usr/bin/env python3
"""aa.py <run logs...>: a set's arms against its first arm, judged against the set's own A/A.

Each log is one set.sh run (its RUN lines and CALIB lines). Runs whose CALIB drifts more than --calib
are left out, as are rows whose steal is above --steal. The first arm's twin (`<arm>~aa`) is the A/A:
the spread S of a metric is max(|lo|, |hi|) of the twin-minus-arm 95% bootstrap CI, and a pair's
shift is real only if its CI excludes 0 and its median difference is beyond S.
"""
import argparse, json, random, statistics as st

TWIN = '~aa'

def load(path):
    cal = [float(l.split()[1]) for l in open(path) if l.startswith('CALIB')]
    rows = [json.loads(l[4:]) for l in open(path) if l.startswith('RUN ')]
    return cal, rows

def boot(x, y, n=10000):
    random.seed(1)
    d = st.median(x) - st.median(y)
    bs = sorted(st.median(random.choices(x, k=len(x))) - st.median(random.choices(y, k=len(y))) for _ in range(n))
    return d, bs[n // 40], bs[n - n // 40]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('logs', nargs='+')
    ap.add_argument('--set', default='main')
    ap.add_argument('--metrics', default='i2pP50,i2pP95')
    ap.add_argument('--calib', type=float, default=0.15)
    ap.add_argument('--steal', type=float, default=2.0)
    a = ap.parse_args()
    metrics = a.metrics.split(',')
    runs = []
    for path in a.logs:
        cal, rows = load(path)
        drift = abs(cal[1] - cal[0]) / cal[0] if len(cal) == 2 else None
        keep = drift is not None and drift <= a.calib
        print(f"{path}: CALIB {' -> '.join(map(str, cal)) or 'missing'}"
              f"{f' ({drift * 100:.1f}%)' if drift is not None else ''}{'' if keep else ' LEFT OUT'}")
        if keep:
            runs.append((path, [r for r in rows if r.get('set') == a.set and all(m in r for m in metrics)]))
    for path, rows in runs:
        for r in rows:
            if (r.get('stealPct') or 0) > a.steal:
                print(f"  dropped {path} {r['arm']} round {r['round']}: steal {r['stealPct']:.2f}%")
    ok = [(p, [r for r in R if (r.get('stealPct') or 0) <= a.steal]) for p, R in runs]
    arms = list(dict.fromkeys(r['arm'] for _, R in ok for r in R))
    twins = [x for x in arms if x.endswith(TWIN)]
    if not twins:
        print('no A/A twin in these runs (AA=0?): nothing to judge against')
        return
    base = twins[0][:-len(TWIN)]
    of = lambda arm, R: [r for r in R if r['arm'] == arm]
    every = [r for _, R in ok for r in R]
    for arm in arms:
        R = of(arm, every)
        extra = ''
        if R and 'keys' in R[0]:
            extra = f"  missed {sum(r['keys'] - r['keysReceived'] for r in R)}/{sum(r['keys'] for r in R)}"
        if R and 'keyframes' in R[0]:
            extra += f"  keyframes {sum(r['keyframes'] for r in R)}"
        print(f"{arm:20} n={len(R)}  " + '  '.join(f"{m} {st.median(r[m] for r in R):.1f}" for m in metrics) + extra)
    others = [x for x in arms if x not in (base, base + TWIN)]
    for m in metrics:
        d, lo, hi = boot([r[m] for r in of(base + TWIN, every)], [r[m] for r in of(base, every)])
        S = max(abs(lo), abs(hi))
        print(f"\n{m}: A/A {base}{TWIN} - {base}: {d:+.1f} [{lo:+.1f}, {hi:+.1f}], spread S = {S:.1f}"
              f"{'  (the A/A CI excludes 0: the bench itself shifts by this much)' if lo > 0 or hi < 0 else ''}")
        for x in others:
            X, B = [r[m] for r in of(x, every)], [r[m] for r in of(base, every)]
            if not X or not B:
                continue
            d, lo, hi = boot(X, B)
            per = []
            for path, R in ok:
                xr, br = [r[m] for r in of(x, R)], [r[m] for r in of(base, R)]
                per.append(f"{st.median(xr) - st.median(br):+.1f}" if xr and br else '-')
            real = (lo > 0 or hi < 0) and abs(d) > S
            verdict = 'REAL ' + ('rise' if d > 0 else 'drop') if real else 'within the A/A'
            print(f"  {x} - {base}: {d:+.1f} [{lo:+.1f}, {hi:+.1f}]  per run {', '.join(per)}  -> {verdict}")

if __name__ == '__main__':
    main()
