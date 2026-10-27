#!/usr/bin/env python3
"""Check i18n key parity across all locale files (ANT-632).

Flattens the full dotted-key set of every ``server/static/i18n/*.json``
file and compares it against ``en.json`` (the reference). The check
fails when any locale diverges from the reference in either direction:

  * keys present in ``en.json`` but missing from the locale, or
  * keys present in the locale but absent from ``en.json``.

Drift accumulated across v0.9.0-v0.9.2 because new UI strings were
added to English only (33 keys missing from all 17 non-English files;
fixed in the v0.9.3 sync, ANT-629). This check keeps the sets pinned
to ``en.json`` going forward.

Empty string values in non-English locales are reported as warnings,
not failures — an untranslated (blanked) key is a translator decision,
not a parity error.

Stdlib only, runs in well under a second.

Usage
-----
    python3 scripts/check-i18n-parity.py
    python3 scripts/check-i18n-parity.py --dir /path/to/i18n

Exit status: 0 = parity holds (warnings allowed), 1 = divergence or
unreadable/missing file.
"""

import argparse
import json
import sys
from pathlib import Path


def flatten(node, prefix=""):
    """Return the set of dotted key paths of every leaf in *node*."""
    keys = set()
    for key, value in node.items():
        path = f"{prefix}.{key}" if prefix else key
        if isinstance(value, dict):
            keys |= flatten(value, path)
        else:
            keys.add(path)
    return keys


def empty_strings(node, prefix=""):
    """Return the dotted paths of every empty/blank string leaf."""
    out = []
    for key, value in node.items():
        path = f"{prefix}.{key}" if prefix else key
        if isinstance(value, dict):
            out.extend(empty_strings(value, path))
        elif isinstance(value, str) and value.strip() == "":
            out.append(path)
    return out


def load(path):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"FAIL: cannot read {path.name}: {exc}", file=sys.stderr)
        sys.exit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--dir",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "server/static/i18n",
        help="directory containing the locale .json files "
        "(default: server/static/i18n in the repo root)",
    )
    args = parser.parse_args()

    i18n_dir = args.dir
    if not i18n_dir.is_dir():
        print(f"FAIL: i18n directory not found: {i18n_dir}", file=sys.stderr)
        return 1

    files = sorted(p for p in i18n_dir.glob("*.json"))
    if not files:
        print(f"FAIL: no .json files in {i18n_dir}", file=sys.stderr)
        return 1

    en_path = i18n_dir / "en.json"
    if en_path not in files:
        print(f"FAIL: reference file {en_path.name} not present in {i18n_dir}",
              file=sys.stderr)
        return 1

    ref = flatten(load(en_path))
    diverged = 0
    warned = 0
    for path in files:
        if path == en_path:
            continue
        data = load(path)
        keys = flatten(data)
        missing = sorted(ref - keys)
        extra = sorted(keys - ref)
        if missing or extra:
            diverged += 1
            if missing:
                print(f"FAIL: {path.name} is missing {len(missing)} key(s) "
                      f"present in en.json:")
                for key in missing:
                    print(f"  - {key}")
            if extra:
                print(f"FAIL: {path.name} has {len(extra)} key(s) not in "
                      f"en.json:")
                for key in extra:
                    print(f"  + {key}")
        else:
            blanks = sorted(empty_strings(data))
            if blanks:
                warned += 1
                print(f"WARN: {path.name} has {len(blanks)} empty string "
                      f"value(s) (untranslated?):")
                for key in blanks:
                    print(f"  ! {key}")

    print(f"\ni18n parity: {len(files)} file(s) checked, reference en.json "
          f"({len(ref)} keys); "
          f"{'OK' if not diverged else str(diverged) + ' locale(s) diverged'}"
          + (f", {warned} locale(s) with empty values" if warned else ""))
    return 1 if diverged else 0


if __name__ == "__main__":
    sys.exit(main())
