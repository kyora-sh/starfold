import json, math, os, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(__file__))
import aa


def log(rows, cal=(1.0, 1.0)):
    f = tempfile.NamedTemporaryFile('w', suffix='.log', delete=False)
    for c in cal[:1]:
        f.write(f'CALIB {c:.3f}\n')
    for r in rows:
        f.write('RUN ' + json.dumps({'set': 'main', **r}) + '\n')
    for c in cal[1:]:
        f.write(f'CALIB {c:.3f}\n')
    f.close()
    return f.name


def rows(arm, n, p50, **extra):
    return [{'arm': arm, 'round': i, 'i2pP50': p50 + i % 3, **extra} for i in range(n)]


class Analysis(unittest.TestCase):
    def test_a_metric_one_arm_lacks_still_judges_the_others(self):
        # noVNC rows have no keyframes: their latency still counts.
        r = rows('novnc', 6, 100) + rows('novnc~aa', 6, 101) + rows('starfold-video', 6, 120, keyframes=0)
        out = aa.analyze([log(r)], metrics=['i2pP50', 'keyframes'])
        self.assertIsNotNone(out['metrics']['i2pP50']['aa'])
        self.assertIn('starfold-video', out['metrics']['i2pP50']['pairs'])
        self.assertIsNone(out['metrics']['keyframes']['aa'])

    def test_null_and_nan_values_are_skipped(self):
        r = rows('a', 6, 100) + rows('a~aa', 6, 100) + rows('b', 6, 100)
        r[0]['i2pP50'] = None
        r[7]['i2pP50'] = math.nan
        out = aa.analyze([log(r)], metrics=['i2pP50'])
        self.assertTrue(all(math.isfinite(v) for v in out['metrics']['i2pP50']['aa']))

    def test_a_baseline_lost_to_steal_does_not_crash(self):
        r = [dict(x, stealPct=5.0) for x in rows('a', 6, 100)] + rows('a~aa', 6, 100) + rows('b', 6, 100)
        out = aa.analyze([log(r)], metrics=['i2pP50'])
        self.assertIsNone(out['metrics']['i2pP50']['aa'])
        self.assertEqual(out['metrics']['i2pP50']['pairs'], {})

    def test_a_row_without_a_steal_reading_is_kept_and_reported(self):
        r = rows('a', 6, 100, stealPct=0.1) + rows('a~aa', 6, 100, stealPct=0.1)
        r[0].pop('stealPct')
        r[1]['stealPct'] = None
        r[2]['stealPct'] = math.nan
        r += [{'arm': 'a', 'round': 9, 'failed': 'x'}, dict(rows('b', 1, 100)[0], stealPct=9.0)]
        path = log(r)
        out = aa.analyze([path], metrics=['i2pP50'])
        self.assertEqual(out['stealUnknown'], {path: 3})
        self.assertIsNotNone(out['metrics']['i2pP50']['aa'])
        self.assertEqual(out['arms'], ['a', 'a~aa'], 'the steal 9 row is dropped')

    def test_a_zero_calib_leaves_that_run_out(self):
        good = log(rows('a', 6, 100) + rows('a~aa', 6, 100))
        zero = log(rows('a', 6, 500) + rows('a~aa', 6, 500), cal=(0.0, 1.8))
        out = aa.analyze([zero, good], metrics=['i2pP50'])
        self.assertEqual([keep for *_, keep in out['notes']], [False, True])
        self.assertLess(abs(out['metrics']['i2pP50']['aa'][0]), 5)

    def test_a_shift_beyond_the_aa_counts_and_one_inside_does_not(self):
        r = rows('a', 12, 100) + rows('a~aa', 12, 100) + rows('far', 12, 150) + rows('near', 12, 101)
        pairs = aa.analyze([log(r)], metrics=['i2pP50'])['metrics']['i2pP50']['pairs']
        self.assertTrue(pairs['far'][-1])
        self.assertFalse(pairs['near'][-1])


def quality(arm, n, ok, t=500, **extra):
    return [{'set': 'quality', 'arm': arm, 'round': i, f'settled{t}Psnr': 30.0 if i < ok else 14.0, **extra} for i in range(n)]


class Usable(unittest.TestCase):
    def test_fisher_matches_known_tables(self):
        self.assertAlmostEqual(aa.fisher(3, 1, 1, 3), 0.4857, places=4)
        self.assertAlmostEqual(aa.fisher(8, 2, 1, 5), 0.0350, places=4)
        self.assertEqual(aa.fisher(0, 0, 0, 0), 1.0)

    def test_a_failed_row_counts_as_not_usable_and_a_stolen_one_is_left_out(self):
        r = quality('a', 4, 2) + quality('a~aa', 4, 2) + [{'set': 'quality', 'arm': 'a', 'round': 9, 'failed': 'timeout'}]
        r += [dict(quality('a', 1, 1)[0], round=10, stealPct=5.0)]
        out = aa.usable([log(r)], points=[500])
        self.assertEqual(out['points'][500]['counts']['a'], (2, 5, 1))

    def test_a_rise_beyond_the_aa_counts_and_a_twin_as_far_off_cancels_it(self):
        far = aa.usable([log(quality('a', 12, 1) + quality('a~aa', 12, 2) + quality('b', 12, 10))], points=[500])
        d, p, real = far['points'][500]['pairs']['b']
        self.assertTrue(real and p < 0.05 and d > 0)
        loud = aa.usable([log(quality('a', 12, 1) + quality('a~aa', 12, 11) + quality('b', 12, 10))], points=[500])
        self.assertFalse(loud['points'][500]['pairs']['b'][-1])

    def test_main_rows_and_other_points_are_not_counted(self):
        r = quality('a', 3, 3) + quality('a~aa', 3, 3) + rows('a', 3, 100) + quality('b', 3, 3, t=1000)
        out = aa.usable([log(r)], points=[500])
        self.assertEqual(out['points'][500]['counts']['a'], (3, 3, 0))
        self.assertEqual(out['points'][500]['counts']['b'], (0, 3, 0))


if __name__ == '__main__':
    unittest.main()
