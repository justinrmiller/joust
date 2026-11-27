"""Tests for scripts/check_pins.py."""

from __future__ import annotations

from pathlib import Path

import check_pins
import pytest


def lock(*packages: tuple[str, str]) -> str:
    """A minimal Cargo.lock / uv.lock with the given (name, version) entries."""
    return "".join(
        f'[[package]]\nname = "{name}"\nversion = "{version}"\n\n'
        for name, version in packages
    )


CARGO = {"lancedb": ["0.41.0"], "lance": ["14.0.2"]}


def test_locked_versions_collects_every_entry() -> None:
    text = lock(("numpy", "2.4.6"), ("lancedb", "0.41.0"), ("numpy", "2.5.3"))
    assert check_pins.locked_versions(text) == {
        "numpy": ["2.4.6", "2.5.3"],
        "lancedb": ["0.41.0"],
    }


def test_matching_versions_pass() -> None:
    # Patch releases may differ, and so may pylance's minor version.
    uv = {"lancedb": ["0.41.3"], "pylance": ["14.1.0"]}
    assert check_pins.find_problems(CARGO, uv) == []


def test_a_lancedb_minor_mismatch_fails_with_the_fix() -> None:
    uv = {"lancedb": ["0.39.0"], "pylance": ["14.0.3"]}
    (problem,) = check_pins.find_problems(CARGO, uv)
    assert "lancedb 0.39.0" in problem
    assert "the same major.minor" in problem
    assert "`lancedb==0.41.*`" in problem


def test_a_pylance_major_mismatch_fails_with_the_fix() -> None:
    uv = {"lancedb": ["0.41.0"], "pylance": ["12.0.0"]}
    (problem,) = check_pins.find_problems(CARGO, uv)
    assert "pylance 12.0.0" in problem
    assert "the same major version" in problem
    assert "`pylance==14.*`" in problem


def test_missing_or_ambiguous_packages_fail() -> None:
    problems = check_pins.find_problems(
        {"lancedb": [], "lance": ["14.0.2", "15.0.0"]}, {"pylance": ["14.0.0"]}
    )
    assert len(problems) == 2
    assert "exactly one `lancedb` crate, found 0 (none)" in problems[0]
    assert "found 2 (14.0.2, 15.0.0)" in problems[1]
    (problem,) = check_pins.find_problems(CARGO, {"pylance": ["14.0.0"]})
    assert "doesn't lock `lancedb`" in problem


def test_the_repository_pins_match() -> None:
    assert check_pins.main([]) == 0


def test_main_reports_drift(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    (tmp_path / "Cargo.lock").write_text(
        lock(("lancedb", "0.41.0"), ("lance", "14.0.2")), encoding="utf-8"
    )
    (tmp_path / "uv.lock").write_text(
        lock(("lancedb", "0.39.0"), ("pylance", "12.0.0")), encoding="utf-8"
    )
    assert check_pins.main(["--root", str(tmp_path)]) == 1
    errors = capsys.readouterr().err
    assert "`lancedb==0.41.*`" in errors
    assert "`pylance==14.*`" in errors


def test_main_reports_a_missing_lock(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    assert check_pins.main(["--root", str(tmp_path)]) == 1
    assert "Cargo.lock" in capsys.readouterr().err
