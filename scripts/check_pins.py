#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Check that the scripts' lancedb and pylance match the Rust crates joust links.

Tables the Python scripts write must use a Lance format joust can read, so the
Python packages locked in ``uv.lock`` must stay on the releases of the Rust
crates locked in ``Cargo.lock``:

* Python ``lancedb`` must have the same major.minor as the ``lancedb`` crate
  (the two are released together, e.g. 0.41.0 and 0.41.0).
* ``pylance`` must have the same major version as the ``lance`` crate
  (e.g. 14.0.3 and 14.0.2).

Usage:
    scripts/check_pins.py [--root PATH]

Exits 1, explaining what to change, when they drift apart. CI runs it on every
pull request.
"""

from __future__ import annotations

import argparse
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

# The repository root (this file is scripts/check_pins.py).
REPO_ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Pin:
    """One Python package that must track one Rust crate."""

    python: str
    rust: str
    # How many leading version components must be equal.
    components: int

    def describe(self) -> str:
        """The rule in words, e.g. "the same major.minor"."""
        return (
            "the same major version" if self.components == 1 else "the same major.minor"
        )

    def specifier(self, rust_version: str) -> str:
        """The pyproject.toml specifier that follows ``rust_version``."""
        prefix = ".".join(rust_version.split(".")[: self.components])
        return f"{self.python}=={prefix}.*"


PINS = (
    Pin(python="lancedb", rust="lancedb", components=2),
    Pin(python="pylance", rust="lance", components=1),
)


def locked_versions(lock_text: str) -> dict[str, list[str]]:
    """Maps each package name in a ``Cargo.lock`` or ``uv.lock`` to its versions.

    Both files are TOML with one ``[[package]]`` table (``name``, ``version``)
    per locked package; a name can appear more than once.
    """
    versions: dict[str, list[str]] = {}
    for package in tomllib.loads(lock_text).get("package", []):
        name, version = package.get("name"), package.get("version")
        if isinstance(name, str) and isinstance(version, str):
            versions.setdefault(name, []).append(version)
    return versions


def leading(version: str, components: int) -> tuple[str, ...]:
    """The first ``components`` dot-separated parts of ``version``."""
    return tuple(version.split(".")[:components])


def find_problems(cargo: dict[str, list[str]], uv: dict[str, list[str]]) -> list[str]:
    """Describes every way the locked Python packages drift from the crates."""
    problems: list[str] = []
    for pin in PINS:
        rust_versions = cargo.get(pin.rust, [])
        python_versions = uv.get(pin.python, [])
        if len(rust_versions) != 1:
            problems.append(
                f"Cargo.lock should lock exactly one `{pin.rust}` crate, found "
                f"{len(rust_versions)} ({', '.join(rust_versions) or 'none'})."
            )
            continue
        (rust_version,) = rust_versions
        if not python_versions:
            problems.append(f"uv.lock doesn't lock `{pin.python}`.")
            continue
        for python_version in python_versions:
            if leading(python_version, pin.components) != leading(
                rust_version, pin.components
            ):
                problems.append(
                    f"uv.lock has {pin.python} {python_version}, but Cargo.lock has "
                    f"the {pin.rust} crate {rust_version}; they need {pin.describe()}. "
                    f"Set `{pin.specifier(rust_version)}` in pyproject.toml, then run "
                    "`uv lock`."
                )
    return problems


def main(argv: list[str] | None = None) -> int:
    """Runs the check and prints the result; returns the exit status."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--root",
        type=Path,
        default=REPO_ROOT,
        help="directory holding Cargo.lock and uv.lock (default: the repository)",
    )
    args = parser.parse_args(argv)
    root: Path = args.root

    try:
        cargo = locked_versions((root / "Cargo.lock").read_text(encoding="utf-8"))
        uv = locked_versions((root / "uv.lock").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        print(f"check_pins.py: {error}", file=sys.stderr)
        return 1

    problems = find_problems(cargo, uv)
    if problems:
        for problem in problems:
            print(f"check_pins.py: {problem}", file=sys.stderr)
        return 1
    summary = ", ".join(
        f"{pin.python} {'/'.join(uv[pin.python])} ~ {pin.rust} {cargo[pin.rust][0]}"
        for pin in PINS
    )
    print(f"Python pins match Cargo.lock: {summary}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
