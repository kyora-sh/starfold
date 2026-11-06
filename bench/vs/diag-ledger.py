"""diag-ledger.py <dir of run dirs>: per key, the first stage where its tag went missing, per arm."""
import collections, json, os, statistics, sys
CAP = 5000
def answer(keys, tags):
    at, res = 0, []
    for t, key in keys:
        v, i = ord(key) - 97, at
        while i < len(tags) and tags[i][0] < t + CAP and not (tags[i][0] >= t and tags[i][1] == v): i += 1
        if i == len(tags) or tags[i][0] >= t + CAP: res.append(None); continue
        at = i + 1
        res.append(tags[i][0] - t)
    return res
def one(d):
    page = json.load(open(f'{d}/page.json'))
    src = [tuple(map(int, l.split())) for l in open(f'{d}/vs-keys.log')]
    idx = [list(map(int, l.split())) for l in open(f'{d}/vs-idx.log')]
    ref = json.load(open(f'{d}/ref.json'))['rows']
    s0 = [r for r in idx if r[6] > 0]
    assert len(s0) == len(ref), (d, len(s0), len(ref))
    sent = [dict(ms=a[0], id=a[1], n=b[0], tag=b[1]) for a, b in zip(s0, ref)]
    client = {r[0]: r for r in page.get('clientDiag') or []}
    samples, keys, org = page['samples'], page['keys'], page['timeOrigin']
    src = src[-len(keys):] if len(src) >= len(keys) else src
    got = answer(keys, page['tags'])
    out = []
    for k, (t, key) in enumerate(keys):
        row = dict(key=key, matched=got[k])
        if k >= len(src) or src[k][1] != ord(key) - 97:
            row['stage'] = 'key never reached source'; out.append(row); continue
        ms, tag, n0 = src[k]
        n1 = src[k + 1][2] if k + 1 < len(src) else 1 << 30
        win = [f for f in sent if n0 <= f['n'] <= n1]
        carry = [f for f in win if f['tag'] == tag]
        row['sourceMs'] = ms - (t + org)
        seen = [s for s in samples if s[2] == tag and (s[1] < 0 or n0 <= s[1] <= n1) and s[0] >= t - 1]
        row['truth'] = seen[0][0] - t if seen else None
        if seen and got[k] is not None: row['stage'] = 'read, matched' if abs(got[k] - row['truth']) < 1 else 'read, matched to wrong tag'
        elif seen: row['stage'] = 'read, matcher missed it'
        elif not win: row['stage'] = 'no frame sent while tag was up'
        elif not carry: row['stage'] = 'sent frames illegible' if any(f['tag'] < 0 for f in win) else 'sent frames carry no tag yet'
        else:
            c = [client.get(f['id']) for f in carry]
            if not any(c): row['stage'] = 'sent, not received'
            elif not any(x and x[2] for x in c): row['stage'] = 'received, not decoded'
            elif any(s[1] in {f['n'] for f in carry} for s in samples): row['stage'] = 'sampled, tag not parsed'
            else: row['stage'] = 'decoded, never sampled'
        out.append(row)
    return out, page
root = sys.argv[1]
arms = collections.defaultdict(lambda: collections.Counter())
perrun = collections.defaultdict(list)
for name in sorted(os.listdir(root), key=lambda x: (x.rsplit('-', 1)[0], int(x.rsplit('-', 1)[1]) if x.rsplit('-', 1)[1].isdigit() else 0)):
    d = f'{root}/{name}'
    if not os.path.exists(f'{d}/ref.json'): print('NO REF', name); continue
    rows, page = one(d)
    arm = name.rsplit('-', 2)[0]
    c = collections.Counter(r['stage'] for r in rows)
    arms[arm].update(c)
    answered = [r['matched'] for r in rows if r['matched'] is not None]
    truth = [r['truth'] for r in rows if r.get('truth') is not None]
    miss = sum(r['matched'] is None for r in rows)
    perrun[arm].append((len(rows), miss, sum(r.get('truth') is None for r in rows)))
    print(f"{name}: keys {len(rows)} answerMissing {miss} trulyUnread {sum(r.get('truth') is None for r in rows)} answerP50 {statistics.median(answered) if answered else -1:.0f} truthP50 {statistics.median(truth) if truth else -1:.0f} bad {page.get('bad')} tagBad {page.get('tagBad')} busy {page.get('busy')} dumps {len(page.get('dumps') or [])} {dict(c)}")
print()
for arm, c in arms.items():
    r = perrun[arm]
    print(arm, 'runs', len(r), 'keys', sum(x[0] for x in r), 'runs with >=10 answerMissing', sum(x[1] >= 10 for x in r), 'runs with >=10 trulyUnread', sum(x[2] >= 10 for x in r))
    for k, v in c.most_common(): print(f'   {v:5d}  {k}')
