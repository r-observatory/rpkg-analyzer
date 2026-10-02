#!/usr/bin/env python3
"""Self-tests of judge.py: python3 ci/memory-ceiling/test_judge.py

Each test writes rows like measure.sh's for a list of one or two inputs, runs the
verdict as the workflow does, and reads the text it prints.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

JUDGE = Path(__file__).resolve().parent / "judge.py"
COLUMNS = (
    "id build limit_mib run exit vmhwm_kb vmpeak_kb maxrss_kb user_s sys_s wall_s oom_kill "
    "cgroup_peak_kb stderr_bytes stdout_bytes sha256 container_exit container_oom"
).split()
MIB = 1024


def run(id_, build, limit, n, mib, exit_=0, oom=0, stderr=0, sha="aaa"):
    kb = int(mib * MIB)
    dead = exit_ != 0
    row = [id_, build, limit, n, exit_, kb, kb + 20 * MIB, kb, 1, 0, 1, oom, kb, stderr]
    return row + [0 if dead else 10, "none" if dead else sha, 0, "true" if oom else "false"]


def rows(id_, cand, base, limits=True):
    """Unlimited runs of both builds at the given peaks in MiB, and the two limit runs."""
    out = [run(id_, "cand", "none", n + 1, m) for n, m in enumerate(cand)]
    out += [run(id_, "base", "none", n + 1, m) for n, m in enumerate(base)]
    if limits:
        out += [run(id_, "cand", lim, 1, cand[0]) for lim in ("3072", "2560")]
    return out


def killed(id_, build, n):
    return run(id_, build, "none", n, 2044, exit_=137, oom=1)


def verdict(all_rows, ids=("quiet@1.0",), extra=()):
    with tempfile.TemporaryDirectory() as d:
        inputs = Path(d) / "inputs.tsv"
        inputs.write_text(
            "set\towner\tpackage\tref\tcommit\tkind\n"
            + "".join(f"heavy\tcran\t{i.split('@')[0]}\t{i.split('@')[1]}\tc0ffee\trelease\n" for i in ids)
        )
        runs = Path(d) / "runs.tsv"
        runs.write_text(
            "\t".join(COLUMNS) + "\n" + "".join("\t".join(str(x) for x in r) + "\n" for r in all_rows)
        )
        cmd = [sys.executable, str(JUDGE), "verdict", "--runs", str(runs), "--inputs", str(inputs)]
        r = subprocess.run(cmd + ["--require", "1,2,4", *extra], capture_output=True, text=True)
    return r.returncode, r.stdout


def line(text, start):
    return next(x.strip() for x in text.splitlines() if x.strip().startswith(start))


class Rise(unittest.TestCase):
    def test_a_real_rise_on_a_quiet_input_fails(self):
        code, text = verdict(rows("quiet@1.0", [520.2, 520.4, 520.1], [500.1, 500.3, 500.2]))
        self.assertEqual(code, 1)
        self.assertIn("FAIL check 2 rise: 1 of 1 inputs", text)
        self.assertIn("largest such rise +19.8 MiB (quiet@1.0)", text)
        own = line(text, "FAIL quiet@1.0")
        self.assertIn("runs 520.1 to 520.4 MiB, baseline runs 500.1 to 500.3 MiB", own)
        self.assertIn("smallest run +19.8 MiB over the baseline's largest", own)

    def test_noise_on_a_noisy_input_passes(self):
        # One build against itself: 917 to 974 beside 868 to 975.
        code, text = verdict(rows("quiet@1.0", [917.3, 974.0, 917.6], [873.3, 868.0, 974.9]))
        self.assertEqual(code, 0)
        self.assertIn("PASS check 2 rise: 0 of 1 inputs", text)
        own = line(text, "PASS quiet@1.0")
        self.assertIn("runs 917.3 to 974.0 MiB, baseline runs 868.0 to 974.9 MiB", own)
        self.assertIn("(smallest against largest -57.6)", own)
        self.assertIn("largest spread within one build 106.9 MiB (quiet@1.0, the baseline)", text)
        self.assertIn("1 inputs above 4 MiB", text)

    def test_one_baseline_run_does_not_cover_that_noise(self):
        # The same peaks with the baseline run once: 917.3 against 873.3.
        code, text = verdict(rows("quiet@1.0", [917.3, 974.0, 917.6], [873.3]))
        self.assertEqual(code, 1)
        self.assertIn("largest such rise +44.0 MiB", text)
        self.assertIn("the baseline ran fewer than 3 times on 1 inputs", text)

    def test_a_real_rise_on_a_noisy_input_fails(self):
        code, text = verdict(rows("quiet@1.0", [1000.0, 1060.0, 1001.0], [873.3, 868.0, 974.9]))
        self.assertEqual(code, 1)
        self.assertIn("smallest run +25.1 MiB over the baseline's largest", line(text, "FAIL quiet@1.0"))

    def test_the_bar_is_sixteen_mib(self):
        code, _ = verdict(rows("quiet@1.0", [516.0, 516.2, 516.1], [500.0, 499.9, 499.8]))
        self.assertEqual(code, 0)
        code, text = verdict(rows("quiet@1.0", [516.1, 516.2, 516.3], [500.0, 499.9, 499.8]))
        self.assertEqual(code, 1)
        self.assertIn("largest such rise +16.1 MiB", text)

    def test_a_quiet_input_keeps_the_short_line(self):
        code, text = verdict(rows("quiet@1.0", [500.4, 500.2, 500.3], [500.1, 500.3, 500.2]))
        self.assertEqual(code, 0)
        self.assertIn("baseline 500.3 MiB (-0.1)", line(text, "PASS quiet@1.0"))
        self.assertNotIn("baseline runs", text)

    def test_a_baseline_that_does_not_fit_is_not_judged(self):
        all_rows = rows("quiet@1.0", [520.0, 520.1, 520.2], [])
        all_rows += [killed("quiet@1.0", "base", n) for n in (1, 2, 3)]
        all_rows += rows("other@2.0", [300.0, 300.1, 300.2], [300.0, 300.1, 300.2])
        code, text = verdict(all_rows, ids=("quiet@1.0", "other@2.0"))
        self.assertEqual(code, 0)
        self.assertIn("baseline does not fit (exit 137, OOM kill)", line(text, "PASS quiet@1.0"))
        self.assertIn("PASS check 2 rise: 0 of 1 inputs", text)
        self.assertIn("baseline fits on 1 of 2 inputs", text)

    def test_only_the_finished_baseline_runs_count(self):
        all_rows = rows("quiet@1.0", [520.0, 520.1, 520.2], [500.0, 500.2])
        all_rows.append(killed("quiet@1.0", "base", 3))
        code, text = verdict(all_rows)
        self.assertEqual(code, 1)
        self.assertIn("baseline runs 500.0 to 500.2 MiB", line(text, "FAIL quiet@1.0"))

    def test_a_build_with_no_finished_run_is_not_judged(self):
        all_rows = [killed("quiet@1.0", "cand", n) for n in (1, 2, 3)]
        all_rows += [run("quiet@1.0", "base", "none", n, 500.0) for n in (1, 2, 3)]
        code, text = verdict(all_rows)
        self.assertEqual(code, 1)
        self.assertIn("rise not judged, no run of the build finished", line(text, "FAIL quiet@1.0"))
        self.assertIn("PASS check 2 rise", text)

    def test_no_baseline_is_not_run(self):
        code, text = verdict(rows("quiet@1.0", [500.0, 500.1, 500.2], []))
        self.assertEqual(code, 1)
        self.assertIn("NOT RUN check 2 rise: no baseline build", text)


class Unchanged(unittest.TestCase):
    def test_a_run_over_the_ceiling_fails_the_ceiling(self):
        code, text = verdict(rows("quiet@1.0", [2100.0, 2100.1, 2100.2], [2100.0, 2100.1, 2100.2]))
        self.assertEqual(code, 1)
        self.assertIn("FAIL check 1 ceiling: 0 of 1 inputs", text)
        self.assertIn("PASS check 2 rise", text)

    def test_a_missing_run_fails_the_ceiling(self):
        code, text = verdict(rows("quiet@1.0", [500.0, 500.1], [500.0, 500.1, 500.2]))
        self.assertEqual(code, 1)
        self.assertIn("[2 of 3 runs]", line(text, "FAIL quiet@1.0"))

    def test_a_different_output_under_the_limit_fails_the_limit(self):
        all_rows = rows("quiet@1.0", [500.0, 500.1, 500.2], [500.0, 500.1, 500.2], limits=False)
        all_rows.append(run("quiet@1.0", "cand", "3072", 1, 500.0, sha="bbb"))
        code, text = verdict(all_rows)
        self.assertEqual(code, 1)
        self.assertIn("FAIL check 4 limit: 0 of 1 inputs", text)


if __name__ == "__main__":
    unittest.main()
