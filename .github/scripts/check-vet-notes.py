#!/usr/bin/env python3
"""Checks the notes of cargo-vet exemptions and trusted entries a pull request adds.

Usage: check-vet-notes.py BASE  (a commit; compared with the working tree)

Fails if supply-chain/config.toml or supply-chain/audits.toml has an [[exemptions.*]] or
[[trusted.*]] entry that BASE does not, and its notes do not name who added it ("by NAME"),
why, and the issue that tracks it (#N), as docs/security/dependency-review.md requires.
An entry counts as added when its crate, criteria and version (or publisher) are new, so
changing an exemption's version or raising its criteria needs fresh notes.
"""
import re
import subprocess
import sys
import tomllib
from pathlib import Path

FILES = ["supply-chain/config.toml", "supply-chain/audits.toml"]
WHO = re.compile(r"\bby \S")
ISSUE = re.compile(r"#\d+\b")
# Fewer words than this cannot say why as well as who and which issue.
MIN_WORDS = 8


def entries(text):
    """Each exemption and trusted entry in a supply-chain file, by identity, with its notes."""
    found = {}
    data = tomllib.loads(text)
    for kind, who in (("exemptions", "version"), ("trusted", "user-id")):
        for crate, items in data.get(kind, {}).items():
            for item in items:
                found[(kind, crate, item.get("criteria"), item.get(who))] = item.get("notes", "")
    return found


def problem(notes):
    if not notes.strip():
        return "has no notes"
    missing = [what for what, pattern in (("who (by NAME)", WHO), ("a tracking issue (#N)", ISSUE))
               if not pattern.search(notes)]
    if len(notes.split()) < MIN_WORDS:
        missing.append("why")
    return "notes do not name " + ", ".join(missing) if missing else None


def check(base_texts, head_texts):
    """Errors for entries in head_texts that base_texts lack. Both map a path to its text."""
    errors = []
    for path, text in head_texts.items():
        old = entries(base_texts.get(path, ""))
        for (kind, crate, criteria, ident), notes in entries(text).items():
            if (kind, crate, criteria, ident) in old:
                continue
            why = problem(notes)
            if why:
                errors.append(f"{path}: new [[{kind}.{crate}]] {ident} ({criteria}) {why}")
    return errors


def at(commit, path):
    shown = subprocess.run(["git", "show", f"{commit}:{path}"], capture_output=True, text=True)
    return shown.stdout if shown.returncode == 0 else ""


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    base = sys.argv[1]
    heads = {path: Path(path).read_text(encoding="utf-8") for path in FILES if Path(path).exists()}
    errors = check({path: at(base, path) for path in FILES}, heads)
    for error in errors:
        print(error)
    if errors:
        sys.exit("Give each new entry notes like: \"Added by NAME because WHY. Review in #N.\"")
    print("No new exemption or trusted entry lacks notes.")


if __name__ == "__main__":
    main()
