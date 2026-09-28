#!/usr/bin/env python3
"""Summarise line coverage of joust's production code from an lcov report.

``cargo llvm-cov`` counts the lines of ``#[cfg(test)]`` modules as covered code,
which inflates the totals. This script drops every line from a file's first
inline ``#[cfg(test)] mod name {`` onwards (joust keeps test modules at the end
of each file), skips whole files declared as ``#[cfg(test)] mod name;``, and
prints per-file and overall line coverage of what remains.

Usage:
    cargo llvm-cov --lcov --output-path target/coverage.lcov
    python3 scripts/coverage.py target/coverage.lcov [--min 80]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# `mod name {` (inline) or `mod name;` (separate file), optionally `pub`.
MOD_DECLARATION = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*(\{|;)")


@dataclass
class FileCoverage:
    """Executable-line hit counts for one source file."""

    path: Path
    hits: dict[int, int] = field(default_factory=dict)

    def production_lines(self) -> dict[int, int]:
        """Hit counts for lines before the file's test module."""
        cutoff = first_test_line(self.path)
        return {line: count for line, count in self.hits.items() if line < cutoff}


def read_lines(path: Path) -> list[str]:
    """Return the file's lines, or none if it cannot be read."""
    try:
        return path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []


def test_modules(lines: list[str]) -> list[tuple[int, str, bool]]:
    """``(line, module name, inline?)`` for each ``#[cfg(test)] mod``."""
    found: list[tuple[int, str, bool]] = []
    for number, line in enumerate(lines, start=1):
        if line.strip() != "#[cfg(test)]":
            continue
        following = next((rest.strip() for rest in lines[number:] if rest.strip()), "")
        match = MOD_DECLARATION.fullmatch(following)
        if match:
            found.append((number, match.group(1), match.group(2) == "{"))
    return found


def first_test_line(path: Path) -> int:
    """Line of the first inline ``#[cfg(test)] mod`` in ``path`` (or infinity)."""
    modules = test_modules(read_lines(path))
    return min((number for number, _, inline in modules if inline), default=sys.maxsize)


def test_only_files(source_root: Path) -> set[Path]:
    """Files declared with ``#[cfg(test)] mod name;`` (test helpers)."""
    files: set[Path] = set()
    for path in source_root.rglob("*.rs"):
        for _, name, is_inline in test_modules(read_lines(path)):
            if is_inline:
                continue
            # `mod x;` in main.rs/lib.rs/mod.rs lives next to it, else in a
            # directory named after the declaring file.
            is_root = path.stem in {"main", "lib", "mod"}
            base = path.parent if is_root else path.with_suffix("")
            files.add((base / f"{name}.rs").resolve())
            files.add((base / name / "mod.rs").resolve())
    return files


def parse_lcov(report: Path, source_root: Path) -> list[FileCoverage]:
    """Read the ``SF``/``DA`` records for files under ``source_root``."""
    files: list[FileCoverage] = []
    current: FileCoverage | None = None
    for raw in report.read_text(encoding="utf-8").splitlines():
        if raw.startswith("SF:"):
            path = Path(raw[3:]).resolve()
            current = FileCoverage(path) if path.is_relative_to(source_root) else None
            if current is not None:
                files.append(current)
        elif raw.startswith("DA:") and current is not None:
            line, count, *_ = raw[3:].split(",")
            current.hits[int(line)] = max(current.hits.get(int(line), 0), int(count))
    return files


def main() -> int:
    """Print the summary; exit non-zero when below ``--min`` percent."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", type=Path, help="lcov file from cargo llvm-cov")
    parser.add_argument(
        "--src",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "src",
        help="source directory to include (default: joust/src)",
    )
    parser.add_argument(
        "--min", type=float, default=0.0, help="fail below this percentage"
    )
    args = parser.parse_args()

    source_root: Path = args.src.resolve()
    helpers = test_only_files(source_root)
    rows: list[tuple[str, int, int]] = []
    for coverage in parse_lcov(args.report, source_root):
        if coverage.path in helpers:
            continue
        lines = coverage.production_lines()
        covered = sum(1 for count in lines.values() if count > 0)
        rows.append((str(coverage.path.relative_to(source_root)), covered, len(lines)))

    rows.sort()
    width = max((len(name) for name, _, _ in rows), default=10)
    print(f"{'file':<{width}}  {'lines':>11}  {'cover':>6}")
    for name, covered, total in rows:
        percent = 100.0 * covered / total if total else 100.0
        print(f"{name:<{width}}  {covered:>5}/{total:<5}  {percent:>5.1f}%")
    covered_total = sum(covered for _, covered, _ in rows)
    line_total = sum(total for _, _, total in rows)
    overall = 100.0 * covered_total / line_total if line_total else 100.0
    label = "TOTAL (production code)"
    print(f"{label:<{width}}  {covered_total:>5}/{line_total:<5}  {overall:>5.1f}%")

    if overall < args.min:
        message = f"coverage {overall:.1f}% is below the minimum of {args.min:.1f}%"
        print(message, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
