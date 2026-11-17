"""Prints results.md's tables from results/*.jsonl. Usage: python3 -I summarize.py"""

import json
import pathlib
import statistics

MB = 1e6


def load(path):
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    return [r for r in rows if r["step"] != "host"], next((r for r in rows if r["step"] == "host"), {})


def pick(rows, corpus, config, step):
    return [r["r"] for r in rows if (r["corpus"], r["config"], r["step"]) == (corpus, config, step)]


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def peak(r):
    # Prototype peak as mem-gate reads it: phys_footprint on macOS, max RSS on Linux.
    return r["footprint"] or r["rss"]


def main():
    here = pathlib.Path(__file__).parent
    for path in sorted((here / "results").glob("*.jsonl")):
        rows, host = load(path)
        corpora = list(dict.fromkeys(r["corpus"] for r in rows))
        uname = host.get("uname", "")
        print(f"## {path.stem}\n\n{uname}; {host.get('git', '')}; {host.get('cpus', '?')} CPUs\n")
        print("| Corpus | Config | Covered MB | Files | First s | Unchanged s | 1% changed s | Prototype peak MB | git peak MB | Store after take MB | After repack MB | Store / covered | Repack s |")
        print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
        gates = {}
        for c in corpora:
            for cfg in ("adr", "recommended"):
                first = pick(rows, c, cfg, "first")
                if not first:
                    continue
                takes = first + pick(rows, c, cfg, "unchanged") + pick(rows, c, cfg, "changed")
                covered = first[-1]["new_bytes"]
                store = pick(rows, c, cfg, "du_take")[0]["kb"] * 1024
                rep = pick(rows, c, cfg, "repack")[0]
                vals = dict(
                    first=med([r["ms"] for r in first]) / 1000,
                    unchanged=med([r["ms"] for r in pick(rows, c, cfg, "unchanged")]) / 1000,
                    changed=med([r["ms"] for r in pick(rows, c, cfg, "changed")]) / 1000,
                    peak=max(peak(r) for r in takes) / MB,
                    ratio=store / covered,
                )
                gates[(c, cfg)] = vals
                print(
                    f"| {c} | {cfg} | {covered / MB:.0f} | {first[-1]['files']:,} | {vals['first']:.2f} | {vals['unchanged']:.2f} | {vals['changed']:.2f} "
                    f"| {vals['peak']:.2f} | {max(r['git_rss'] for r in takes) / MB:.0f} | {store / MB:.0f} | {rep['kb'] * 1024 / MB:.0f} "
                    f"| {vals['ratio']:.2f} | {rep['t']['ms'] / 1000:.1f} |"
                )
        print("\n| Corpus | Tree MB (without .git) | `cp` clone s | clone du MB | clone df MB | plain `cp` s | plain du MB |")
        print("| --- | --- | --- | --- | --- | --- | --- |")
        for c in corpora:
            tree = pick(rows, c, "-", "tree")[0]
            cl, pl = pick(rows, c, "-", "cp_clone"), pick(rows, c, "-", "cp_plain")
            print(
                f"| {c} | {(tree['kb'] - tree['git_kb']) * 1024 / MB:.0f} | {med([r['t']['ms'] for r in cl]) / 1000:.2f} | {cl[-1]['du_kb'] * 1024 / MB:.0f} "
                f"| {med([r['df_kb'] for r in cl]) * 1024 / MB:.0f} | {med([r['t']['ms'] for r in pl]) / 1000:.2f} | {pl[-1]['du_kb'] * 1024 / MB:.0f} |"
            )
        print("\n| Corpus | Default caps: complete | Files | New MB | s |")
        print("| --- | --- | --- | --- | --- |")
        for c in corpora:
            d = pick(rows, c, "default", "default_caps")[0]
            print(f"| {c} | {d['complete']} | {d['files']:,} | {d['new_bytes'] / MB:.0f} | {d['ms'] / 1000:.2f} |")
        print("\n| Corpus | Tracked files | Index read s | First take s | Estimated first take from `.git/index` s | Saved s |")
        print("| --- | --- | --- | --- | --- | --- |")
        for c in corpora:
            ix = pick(rows, c, "-", "index")
            if ix and (c, "recommended") in gates:
                g = gates[(c, "recommended")]
                est = g["unchanged"] + ix[0]["t"]["ms"] / 1000
                print(f"| {c} | {ix[0]['tracked']:,} | {ix[0]['t']['ms'] / 1000:.2f} | {g['first']:.2f} | {est:.2f} | {g['first'] - est:.2f} |")
        g = gates.get(("linux", "recommended"))
        a = gates.get(("linux", "adr"))
        if g and a:
            worst = max(v["peak"] for (c, cfg), v in gates.items() if cfg == "recommended")
            print("\n| Gate | ADR as written | Recommended |")
            print("| --- | --- | --- |")
            verdict = lambda ok: "pass" if ok else "**fail**"  # noqa: E731
            print(f"| Linux tree, first take ≤ 10 s | {a['first']:.2f} s, {verdict(a['first'] <= 10)} | {g['first']:.2f} s, {verdict(g['first'] <= 10)} |")
            print(f"| Linux tree, unchanged ≤ 1 s | {a['unchanged']:.2f} s, {verdict(a['unchanged'] <= 1)} | {g['unchanged']:.2f} s, {verdict(g['unchanged'] <= 1)} |")
            print(f"| Linux tree, store ≤ half of covered | {a['ratio']:.2f}, {verdict(a['ratio'] <= 0.5)} | {g['ratio']:.2f}, {verdict(g['ratio'] <= 0.5)} |")
            aw = max(v["peak"] for (c, cfg), v in gates.items() if cfg == "adr")
            print(f"| Prototype ≤ 4 MB, every corpus | {aw:.2f} MB, {verdict(aw <= 4)} | {worst:.2f} MB, {verdict(worst <= 4)} |")
        print()


main()
