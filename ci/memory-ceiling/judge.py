#!/usr/bin/env python3
"""Judge the memory measurements of measure.sh and worker.sh.

  judge.py verdict --runs runs.tsv [...] [--worker DIR] [--other NAME=sha.tsv ...]
  judge.py largest --runs runs.tsv [...]

verdict applies five checks and prints PASS or FAIL on the first line:
  1 ceiling    every run of the build exits 0 with empty stderr, no OOM kill, and a
               resident peak (VmHWM) at or under the ceiling
  2 rise       the build's smallest finished run is no more than a few MiB above
               the baseline build's largest finished run, where the baseline fits.
               Identical runs of one build differ, so each build's own range is
               the noise the comparison has to clear
  3 worker     the real worker runs: every package ok, no OOM kill, and available
               memory never under the floor
  4 limit      same output and exit 0 under the address-space limit
  5 platforms  same output as the same build on another platform
largest prints the four Bioconductor packages with the largest resident peaks, the
list worker.sh runs first, and exits 3 when one of them did not stay under the ceiling.
"""

import argparse
import csv
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
NAMES = {1: "ceiling", 2: "rise", 3: "worker", 4: "limit", 5: "platforms"}


def mib(kb):
    return f"{kb / 1024:,.1f}"


def signed(kb):
    return f"{round(kb / 1024, 1) + 0.0:+,.1f}"


def load_inputs(path):
    with open(path, newline="") as f:
        return [dict(r, id=f"{r['package']}@{r['ref']}") for r in csv.DictReader(f, delimiter="\t")]


def load_runs(paths):
    by_id = {}
    for p in paths:
        with open(p, newline="") as f:
            for r in csv.DictReader(f, delimiter="\t"):
                for k in ("exit", "vmhwm_kb", "vmpeak_kb", "maxrss_kb", "oom_kill", "stderr_bytes", "run"):
                    r[k] = int(r[k])
                by_id.setdefault(r["id"], []).append(r)
    return by_id


def resident(r):
    """VmHWM, or the wait4 figure when the kernel gave no exit stop (a kill)."""
    return r["vmhwm_kb"] if r["vmhwm_kb"] > 0 else max(r["maxrss_kb"], 0)


def killed(r):
    return r["oom_kill"] > 0 or r["container_oom"] == "true"


def faults(r, ceiling_kb=None):
    """Why one run fails, as a list of short phrases."""
    out = []
    if r["exit"] != 0:
        out.append(f"exit {r['exit']}")
    if killed(r):
        out.append("OOM kill")
    elif r["oom_kill"] < 0:
        out.append("OOM count unreadable")
    if r["stderr_bytes"] > 0:
        out.append(f"stderr {r['stderr_bytes']} bytes")
    if ceiling_kb is not None and resident(r) > ceiling_kb:
        out.append(f"VmHWM {mib(resident(r))} MiB")
    return out


def peaks(rows):
    """The resident peaks of the runs that finished: exit 0, no OOM kill, empty stderr."""
    return [resident(r) for r in rows if not faults(r)]


def span(kbs):
    """A build's range over its finished runs, as text."""
    lo, hi = mib(min(kbs)), mib(max(kbs))
    return lo if lo == hi else f"{lo} to {hi}"


class Input:
    """One input's rows, sorted into the build's runs, the baseline's runs and the limit runs."""

    def __init__(self, rows):
        self.fetch_failed = any(r["build"] == "fetch" for r in rows)
        self.cand = sorted(
            (r for r in rows if r["build"] == "cand" and r["limit_mib"] == "none"), key=lambda r: r["run"]
        )
        self.base = sorted(
            (r for r in rows if r["build"] == "base" and r["limit_mib"] == "none"), key=lambda r: r["run"]
        )
        self.cand_peaks, self.base_peaks = peaks(self.cand), peaks(self.base)
        self.limits = {
            int(r["limit_mib"]): r for r in rows if r["build"] == "cand" and r["limit_mib"] != "none"
        }
        self.hwm = max((resident(r) for r in self.cand), default=0)
        self.vmpeak = max((max(r["vmpeak_kb"], 0) for r in self.cand), default=0)
        ok = [r for r in self.cand if r["exit"] == 0]
        self.sha = ok[0]["sha256"] if ok else None

    def ceiling_faults(self, a):
        if self.fetch_failed:
            return ["tree not fetched"]
        out = []
        if len(self.cand) < a.run_count:
            out.append(f"{len(self.cand)} of {a.run_count} runs")
        for r in self.cand:
            out += [f"run {r['run']} {x}" for x in faults(r, a.ceiling_mib * 1024)]
        if len({r["sha256"] for r in self.cand}) > 1:
            out.append("output differs between runs")
        return out

    def clean(self, a):
        return bool(self.cand) and not self.ceiling_faults(a)


def check_inputs(a, inputs, runs):
    """Checks 1, 2 and 4, one line per input."""
    lines, rows = [], []
    status = {1: "PASS", 2: "PASS", 4: "PASS"}
    summary = {}
    not_run = [i["id"] for i in inputs if i["id"] not in runs]
    measured = [(i["id"], Input(runs[i["id"]])) for i in inputs if i["id"] in runs]
    over, rises, limit_bad, base_seen, base_fit, limit_seen = [], [], [], 0, 0, 0
    risen, spreads, base_short = [], [], 0
    for id_, x in measured:
        bad = x.ceiling_faults(a)
        if bad:
            over.append(id_)
        text = [f"VmHWM {mib(x.hwm)} MiB, VmPeak {mib(x.vmpeak)} MiB"]
        row = {"id": id_, "hwm": mib(x.hwm), "vmpeak": mib(x.vmpeak), "base": "", "rise": "", "limits": {}}
        # 2: the build's smallest finished run against the baseline's largest.
        spread = 0
        for build, kbs in (("the build", x.cand_peaks), ("the baseline", x.base_peaks)):
            if len(kbs) > 1:
                spreads.append((max(kbs) - min(kbs), id_, build))
                spread = max(spread, max(kbs) - min(kbs))
        noisy = spread > a.noise_mib * 1024
        if noisy and x.cand_peaks:
            row["hwm"] = span(x.cand_peaks)
        if x.base:
            base_seen += 1
            base_short += len(x.base) < a.run_count
            if not x.base_peaks:
                text.append(f"baseline does not fit ({', '.join(faults(x.base[0]))})")
                row["base"] = "does not fit"
            elif not x.cand_peaks:
                base_fit += 1
                row["base"] = span(x.base_peaks)
                text.append(
                    f"baseline {span(x.base_peaks)} MiB, rise not judged, no run of the build finished"
                )
            else:
                base_fit += 1
                rise = min(x.cand_peaks) - max(x.base_peaks)
                rises.append((rise, id_))
                row["rise"] = signed(rise)
                if rise > a.rise_mib * 1024 or noisy:
                    row["hwm"], row["base"] = span(x.cand_peaks), span(x.base_peaks)
                    text.append(
                        f"runs {span(x.cand_peaks)} MiB, baseline runs {span(x.base_peaks)} MiB "
                        f"(smallest against largest {signed(rise)})"
                    )
                else:
                    row["base"] = mib(max(x.base_peaks))
                    text.append(f"baseline {mib(max(x.base_peaks))} MiB ({signed(rise)})")
                if rise > a.rise_mib * 1024:
                    risen.append(id_)
                    bad.append(f"smallest run {signed(rise)} MiB over the baseline's largest")
                    status[2] = "FAIL"
        if noisy and len(x.cand_peaks) > 1 and not any(t.startswith("runs ") for t in text):
            text.append(f"runs {span(x.cand_peaks)} MiB")
        # 4: the address-space limits. Only the first is judged.
        for n, lim in enumerate(a.limits):
            r = x.limits.get(lim)
            if r is None:
                continue
            limit_seen += n == 0
            why = faults(r)
            if not why and x.sha is None:
                why = ["no unlimited run finished, nothing to compare the output with"]
            elif not why and r["sha256"] != x.sha:
                why = ["output differs"]
            row["limits"][lim] = "ok" if not why else ", ".join(why)
            text.append(f"{lim:,} MiB limit {'ok' if not why else ', '.join(why)}")
            if why and n == 0:
                limit_bad.append(id_)
                bad.append(f"{lim:,} MiB limit: {', '.join(why)}")
        row["result"] = "PASS" if not bad else "FAIL"
        rows.append(row)
        lines.append(f"{row['result']} {id_}: {'; '.join(text)}" + (f" [{'; '.join(bad)}]" if bad else ""))
    if a.partial and not_run:
        lines.append(f"information: {len(not_run)} inputs of the list not run here")
    else:
        lines += [f"FAIL {id_}: not run" for id_ in not_run]

    top = max(measured, key=lambda m: m[1].hwm, default=None)
    if not measured:
        status[1] = "NOT RUN"
        summary[1] = "no input measured"
    else:
        if over or (not_run and not a.partial):
            status[1] = "FAIL"
        summary[1] = (
            f"{len(measured) - len(over)} of {len(measured)} inputs at or under "
            f"{a.ceiling_mib:,} MiB with exit 0, empty stderr and no OOM kill"
            + (f", {len(not_run)} not run" if not_run else "")
            + f"; largest VmHWM {mib(top[1].hwm)} MiB ({top[0]})"
        )
    if not base_seen:
        status[2] = "NOT RUN"
        summary[2] = "no baseline build"
    elif rises:
        rise, id_ = max(rises)
        summary[2] = (
            f"{len(risen)} of {len(rises)} inputs have their smallest finished run more than "
            f"{a.rise_mib} MiB above the baseline's largest finished run; largest such rise "
            f"{signed(rise)} MiB ({id_}); baseline fits on {base_fit} of {base_seen} inputs"
        )
    else:
        summary[2] = f"baseline fits on 0 of {base_seen} inputs, nothing to compare"
    if base_seen and spreads:
        wide, id_, build = max(spreads)
        noisy_ids = {i for s, i, _ in spreads if s > a.noise_mib * 1024}
        summary[2] += (
            f"; largest spread within one build {mib(wide)} MiB ({id_}, {build}), "
            f"{len(noisy_ids)} inputs above {a.noise_mib} MiB"
        )
    if base_short:
        summary[2] += f"; the baseline ran fewer than {a.run_count} times on {base_short} inputs"
    if not limit_seen:
        status[4] = "NOT RUN"
        summary[4] = "no limit run"
    else:
        if limit_bad or limit_seen < len(measured):
            status[4] = "FAIL"
        clean = [(i, x) for i, x in measured if x.clean(a) and x.hwm > 0]
        vm = max(clean, key=lambda m: m[1].vmpeak, default=None)
        ratio = max(clean, key=lambda m: m[1].vmpeak / m[1].hwm, default=None)
        summary[4] = (
            f"{limit_seen - len(limit_bad)} of {limit_seen} inputs exit 0 with the same output "
            f"under {a.limits[0]:,} MiB"
        )
        if limit_seen < len(measured):
            summary[4] += f", {len(measured) - limit_seen} without a limit run"
        if vm:
            summary[4] += (
                f"; largest VmPeak {mib(vm[1].vmpeak)} MiB ({vm[0]}); largest VmPeak to VmHWM "
                f"ratio {ratio[1].vmpeak / ratio[1].hwm:.2f} ({ratio[0]})"
            )
            big = [m for m in clean if m[1].hwm >= 512 * 1024]
            if big:
                ratio = max(big, key=lambda m: m[1].vmpeak / m[1].hwm)
                summary[4] += (
                    f", and {ratio[1].vmpeak / ratio[1].hwm:.2f} ({ratio[0]}) among inputs of 512 MiB or more"
                )
        for lim in a.limits[1:]:
            seen = [x.limits[lim] for _, x in measured if lim in x.limits]
            good = [r for r in seen if not faults(r)]
            summary[4] += f"; for information, {len(good)} of {len(seen)} exit 0 under {lim:,} MiB"
    return status, summary, lines, rows, dict(measured)


def read_meta(path):
    meta = {}
    for line in path.read_text().splitlines():
        k, _, v = line.partition(" ")
        meta[k] = v
    return meta


def largest_sampled(path):
    """The largest resident peak sample.sh saw on an analyzer process, in MiB."""
    top = 0
    if path.exists():
        for line in path.read_text().splitlines():
            f = line.split("\t")
            if len(f) == 6 and "analyzer" in f[3]:
                top = max(top, int(f[4]))
    return mib(top)


def check_worker(a):
    """Check 3, one line per package of each list."""
    if not a.worker:
        return "NOT RUN", "no worker run", [], []
    status, summary, lines, tables = "PASS", [], [], []
    for name in a.worker_lists:
        d = Path(a.worker) / name
        if (d / "skipped.txt").exists():
            status = "FAIL"
            summary.append(f"{name} not run")
            lines.append(f"FAIL worker list {name}: not run, {(d / 'skipped.txt').read_text().strip()}")
            continue
        if not (d / "worker.tsv").exists() or not (d / "meta.txt").exists():
            status = "FAIL"
            summary.append(f"{name} not run")
            lines.append(f"FAIL worker list {name}: no result")
            continue
        meta = read_meta(d / "meta.txt")
        with open(d / "worker.tsv", newline="") as f:
            rows = list(csv.DictReader(f, delimiter="\t"))
        bad = []
        for r in rows:
            ok = r["ok"] == "TRUE"
            if not ok:
                bad.append(f"{r['package']} {r['stage']}")
            elif r["versions"] != r["analyzer_versions"]:
                ok = False
                bad.append(
                    f"{r['package']} analyzer read {r['analyzer_versions']} of {r['versions']} releases"
                )

            def m(key, r=r):
                return "unknown" if r[key] == "NA" else f"{float(r[key]) / 1024:,.0f}"

            ser = (
                "unknown" if r["serialized_bytes"] == "NA" else f"{float(r['serialized_bytes']) / 2**20:,.1f}"
            )
            what = (
                f"{r['versions']} releases" if r["ok"] == "TRUE" else f"failed at {r['stage']}: {r['reason']}"
            )
            lines.append(
                f"{'PASS' if ok else 'FAIL'} worker list {name}, {r['package']}: "
                f"{what}, worker VmHWM {m('hwm_exit_kb')} MiB at exit and "
                f"{m('hwm_return_kb')} MiB before its result was sent, result {ser} MiB serialized"
            )
            tables.append(
                (
                    name,
                    r["package"],
                    r["versions"],
                    m("hwm_return_kb"),
                    m("hwm_exit_kb"),
                    ser,
                    "ok" if ok else "failed",
                )
            )
        avail = float(meta.get("min_available_kb", "nan"))
        # Inside a limited container the count is the container's own. The machine-wide
        # count would include whatever else runs beside it.
        limited = meta.get("cgroup_memory_max", "none").isdigit()
        kills = meta.get("cgroup_oom_kills" if limited else "oom_kills", "NA")
        if kills != "0":
            bad.append(f"OOM kills {kills}")
        if meta.get("kernel_log_oom_lines", "0") != "0":
            bad.append("OOM lines in the kernel log")
        if not avail >= a.floor_mib * 1024:
            bad.append(f"available memory fell to {mib(avail)} MiB")
        line = (
            f"{name}: minimum available {mib(avail)} MiB of {mib(float(meta['memtotal_kb']))} MiB, "
            f"OOM kills {kills}, {meta.get('cores')} cores, {meta.get('elapsed_s')} s, "
            f"largest analyzer VmHWM sampled {largest_sampled(d / 'procs.tsv')} MiB, "
            f"parent R VmHWM {mib(float(meta.get('parent_hwm_kb', 'nan')))} MiB with every result held, "
            f"kernel log {meta.get('kernel_log')}"
        )
        if limited:
            line += (
                f", in a container limited to {mib(int(meta['cgroup_memory_max']) / 1024)} MiB "
                f"(peak {mib(float(meta.get('cgroup_peak_kb', 'nan')))} MiB, "
                f"machine-wide OOM kills {meta.get('oom_kills')})"
            )
        if (d / "given.txt").exists():
            line += ", a list given by hand and not the standing one"
        if bad:
            status = "FAIL"
            line += f" [{'; '.join(bad)}]"
        summary.append(line)
    if status == "FAIL" and a.worker_advisory:
        status = "WARN"
    return status, "; ".join(summary), lines, tables


def check_platforms(a, measured):
    """Check 5: this result's output against each other platform's table of id and sha256."""
    if not a.other:
        return "NOT RUN", "no other platform given", []
    lines, differs, compared = [], [], set()
    for pair in a.other:
        name, _, path = pair.partition("=")
        with open(path) as f:
            other = dict(line.split()[:2] for line in f if line.strip())
        for id_, x in measured.items():
            if id_ not in other or x.sha is None:
                continue
            compared.add(id_)
            if other[id_] != x.sha:
                differs.append(id_)
                lines.append(f"FAIL {id_}: output differs from {name}")
    missing = [i for i in measured if i not in compared]
    for id_ in missing:
        lines.append(f"FAIL {id_}: not compared with another platform")
    status = "PASS" if compared and not differs and not missing else "FAIL"
    line = (
        f"{len(compared) - len(set(differs))} of {len(measured)} inputs print the same bytes as on "
        f"{', '.join(s.partition('=')[0] for s in a.other)}"
        + (f", {len(set(differs))} differ" if differs else "")
        + (f", {len(missing)} not compared" if missing else "")
    )
    return status, line, lines


def verdict(a):
    inputs = load_inputs(a.inputs)
    runs = load_runs(a.runs)
    status, summary, lines, rows, measured = check_inputs(a, inputs, runs)
    status[3], summary[3], worker_lines, worker_tables = check_worker(a)
    status[5], summary[5], platform_lines = check_platforms(a, measured)
    lines += worker_lines + platform_lines
    required = [int(c) for c in a.require.split(",")]
    passed = all(status[c] in ("PASS", "WARN") for c in required)
    platform = "unknown platform"
    if a.platform and Path(a.platform).exists():
        p = read_meta(Path(a.platform))
        platform = (
            f"linux {p.get('arch')} glibc {p.get('glibc')}, {p.get('candidate', '').split(' sha256')[0]}"
        )
        lines.append(f"information: {'; '.join(f'{k} {v}' for k, v in p.items())}")
    head = (
        f"{'PASS' if passed else 'FAIL'}  {a.label} on {platform}, {len(measured)} of {len(inputs)} "
        f"inputs: "
        + ", ".join(f"{NAMES[c]} {status[c]}" for c in sorted(status))
        + f" (required: {', '.join(NAMES[c] for c in required)})"
    )
    body = [f"{status[c]} check {c} {NAMES[c]}: {summary[c]}" for c in sorted(status)]
    text = head + "\n" + "".join(f"      {x}\n" for x in body + lines)
    if a.out:
        Path(a.out).write_text(text)
    if a.sha_out:
        Path(a.sha_out).write_text("".join(f"{i}\t{x.sha}\n" for i, x in measured.items() if x.sha))
    if a.markdown:
        with open(a.markdown, "a") as f:
            f.write(markdown(a, head, body, rows, worker_tables))
    print(text, end="")
    return 0 if passed else 1


def markdown(a, head, body, rows, worker_tables):
    out = [f"### {head}\n\n"] + [f"- {x}\n" for x in body]
    limits = "".join(f" {lim:,} MiB limit |" for lim in a.limits)
    out.append(
        "\n| Input | VmHWM MiB | VmPeak MiB | Baseline VmHWM MiB | Smallest over baseline's largest MiB |"
        f"{limits} Result |\n"
    )
    out.append("|---|---:|---:|---:|---:|" + "---|" * len(a.limits) + "---|\n")
    for r in rows:
        cells = "".join(f" {r['limits'].get(lim, '')} |" for lim in a.limits)
        out.append(
            f"| {r['id']} | {r['hwm']} | {r['vmpeak']} | {r['base']} | {r['rise']} |{cells} {r['result']} |\n"
        )
    if worker_tables:
        out.append(
            "\n| Worker list | Package | Releases | VmHWM before send MiB | VmHWM at exit MiB "
            "| Result serialized MiB | Result |\n|---|---|---:|---:|---:|---:|---|\n"
        )
        out += [f"| {' | '.join(t)} |\n" for t in worker_tables]
    return "".join(out) + "\n"


def largest(a):
    owner = {i["id"]: (i["owner"], i["package"]) for i in load_inputs(a.inputs)}
    peak, unclean = {}, set()
    for id_, rows in load_runs(a.runs).items():
        own, pkg = owner.get(id_, (None, None))
        if own != "bioc":
            continue
        x = Input(rows)
        peak[pkg] = max(peak.get(pkg, 0), x.hwm)
        if not x.clean(a):
            unclean.add(pkg)
    top = sorted(peak, key=lambda p: -peak[p])[:4]
    if not top:
        print("no Bioconductor input measured", file=sys.stderr)
        return 3
    bad = [p for p in top if p in unclean]
    if bad:
        print(f"{', '.join(bad)} did not stay under {a.ceiling_mib:,} MiB alone", file=sys.stderr)
        return 3
    print(" ".join(top))
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command", choices=["verdict", "largest"])
    p.add_argument("--runs", nargs="+", required=True)
    p.add_argument("--inputs", default=str(HERE / "inputs.tsv"))
    p.add_argument("--platform", help="platform.txt of the measurement")
    p.add_argument("--worker", help="directory holding one directory per worker list")
    p.add_argument("--worker-lists", nargs="+", default=["largest", "block"])
    p.add_argument("--worker-advisory", action="store_true", help="a worker failure warns, it does not fail")
    p.add_argument("--other", action="append", help="NAME=FILE, another platform's table of id and sha256")
    p.add_argument("--require", default="1,2,3,4", help="checks that must pass")
    p.add_argument("--partial", action="store_true", help="inputs that were not run do not fail")
    p.add_argument("--label", default="memory ceiling")
    p.add_argument("--out")
    p.add_argument("--sha-out")
    p.add_argument("--markdown")
    p.add_argument("--run-count", type=int, default=3, help="unlimited runs of each build per input")
    p.add_argument("--ceiling-mib", type=int, default=2048)
    p.add_argument("--rise-mib", type=int, default=16)
    p.add_argument("--noise-mib", type=int, default=4, help="a spread above this prints both ranges")
    p.add_argument("--floor-mib", type=int, default=2048)
    p.add_argument("--limits", type=int, nargs="+", default=[3072, 2560])
    a = p.parse_args()
    return verdict(a) if a.command == "verdict" else largest(a)


if __name__ == "__main__":
    sys.exit(main())
