"""Tests for scripts/release.py: versions, version bumps and the changelog."""

from __future__ import annotations

import datetime
import re
import textwrap

import pytest
import release
from release import ReleaseError, Version

DATE = datetime.date(2026, 12, 1)

CHANGELOG = textwrap.dedent(
    """\
    # Changelog

    Intro text.

    ## [Unreleased]

    ### Added

    - A new thing (#12).

    ## [0.1.0] - 2026-09-28

    ### Added

    - The first things.

    [Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD
    [0.1.0]: https://github.com/o/r/releases/tag/v0.1.0
    """
)


# Versions ---------------------------------------------------------------------


@pytest.mark.parametrize("text", ["0.1.0", "1.20.300", "1.0.0-rc.1", "1.0.0-x-y.0.z9"])
def test_parse_round_trips(text: str) -> None:
    assert str(Version.parse(text)) == text


@pytest.mark.parametrize(
    "text", ["1.2", "v1.2.3", "01.2.3", "1.2.3-", "1.2.3-01", "1.2.3+build", " 1.2.3"]
)
def test_parse_rejects_non_semver(text: str) -> None:
    with pytest.raises(ReleaseError, match="isn't a version"):
        Version.parse(text)


def test_precedence_follows_semver() -> None:
    # The example ordering from SemVer 2.0, item 11.
    ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "2.0.0",
    ]
    versions = [Version.parse(text) for text in ordered]
    assert sorted(reversed(versions)) == versions
    assert Version.parse("0.10.0") > Version.parse("0.9.9")
    assert Version.parse("1.0.0") == Version.parse("1.0.0")


# Cargo.toml and Cargo.lock -----------------------------------------------------


def test_bump_manifest_changes_only_the_package_version() -> None:
    manifest = textwrap.dedent(
        """\
        [package]
        name = "joust"
        version = "0.1.0"
        edition = "2024"

        [dependencies]
        anyhow = { version = "1" }
        version = "not the package's"
        """
    )
    text, old = release.bump_manifest(manifest, Version.parse("0.2.0"))
    assert old == Version.parse("0.1.0")
    assert text == manifest.replace('version = "0.1.0"', 'version = "0.2.0"')


def test_bump_manifest_needs_a_package_version() -> None:
    with pytest.raises(ReleaseError, match=re.escape("no [package] table")):
        release.bump_manifest(
            '[workspace]\nversion = "1.0.0"\n', Version.parse("1.0.1")
        )
    with pytest.raises(ReleaseError, match="has no version"):
        release.bump_manifest(
            '[package]\nname = "joust"\n\n[dependencies]\nversion = "1.0.0"\n',
            Version.parse("1.0.1"),
        )


LOCK = textwrap.dedent(
    """\
    [[package]]
    name = "itoa"
    version = "0.1.0"

    [[package]]
    name = "joust"
    version = "0.1.0"
    dependencies = [
     "itoa",
    ]
    """
)


def test_bump_lock_changes_only_joust() -> None:
    text = release.bump_lock(LOCK, Version.parse("0.1.0"), Version.parse("0.2.0"))
    assert text == LOCK.replace(
        'name = "joust"\nversion = "0.1.0"', 'name = "joust"\nversion = "0.2.0"'
    )


def test_bump_lock_refuses_an_out_of_sync_lock() -> None:
    with pytest.raises(ReleaseError, match=re.escape("Cargo.lock has joust 0.1.0")):
        release.bump_lock(LOCK, Version.parse("0.1.5"), Version.parse("0.2.0"))
    with pytest.raises(ReleaseError, match="found 0"):
        release.bump_lock("", Version.parse("0.1.0"), Version.parse("0.2.0"))


# CHANGELOG.md --------------------------------------------------------------------


def test_roll_changelog_moves_unreleased_into_a_dated_section() -> None:
    rolled = release.roll_changelog(CHANGELOG, Version.parse("0.2.0"), DATE)
    assert rolled == textwrap.dedent(
        """\
        # Changelog

        Intro text.

        ## [Unreleased]

        ## [0.2.0] - 2026-12-01

        ### Added

        - A new thing (#12).

        ## [0.1.0] - 2026-09-28

        ### Added

        - The first things.

        [Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD
        [0.2.0]: https://github.com/o/r/compare/v0.1.0...v0.2.0
        [0.1.0]: https://github.com/o/r/releases/tag/v0.1.0
        """
    )
    # The rolled changelog can be rolled again once it has new entries.
    again = rolled.replace(
        "## [Unreleased]\n\n", "## [Unreleased]\n\n- Another thing.\n\n", 1
    )
    assert "compare/v0.2.0...v0.3.0" in release.roll_changelog(
        again, Version.parse("0.3.0"), DATE
    )


def test_roll_changelog_refuses_an_empty_unreleased_section() -> None:
    empty = CHANGELOG.replace("### Added\n\n- A new thing (#12).\n\n", "", 1)
    with pytest.raises(ReleaseError, match="Unreleased section is empty"):
        release.roll_changelog(empty, Version.parse("0.2.0"), DATE)
    # Only link definitions left (Unreleased is the last section) is empty too.
    only_links = (
        "# Changelog\n\n## [Unreleased]\n\n[Unreleased]: https://x/compare/a...HEAD\n"
    )
    with pytest.raises(ReleaseError, match="Unreleased section is empty"):
        release.roll_changelog(only_links, Version.parse("0.2.0"), DATE)


def test_roll_changelog_refuses_a_repeated_or_missing_section() -> None:
    with pytest.raises(
        ReleaseError, match=re.escape("already has a section for 0.1.0")
    ):
        release.roll_changelog(CHANGELOG, Version.parse("0.1.0"), DATE)
    with pytest.raises(ReleaseError, match=re.escape("no `## [Unreleased]` section")):
        release.roll_changelog(
            "# Changelog\n\n## [0.1.0]\n\n- x\n", Version.parse("0.2.0"), DATE
        )


def test_release_notes_returns_the_section_body() -> None:
    assert release.release_notes(CHANGELOG, Version.parse("0.1.0")) == (
        "### Added\n\n- The first things.\n"
    )
    rolled = release.roll_changelog(CHANGELOG, Version.parse("0.2.0"), DATE)
    assert release.release_notes(rolled, Version.parse("0.2.0")) == (
        "### Added\n\n- A new thing (#12).\n"
    )


def test_release_notes_needs_a_non_empty_section() -> None:
    with pytest.raises(ReleaseError, match=re.escape("no `## [0.3.0]` section")):
        release.release_notes(CHANGELOG, Version.parse("0.3.0"))
    with pytest.raises(ReleaseError, match=re.escape("section for 0.2.0 is empty")):
        release.release_notes("## [0.2.0] - 2026-12-01\n\n", Version.parse("0.2.0"))


def test_the_repository_changelog_is_well_formed() -> None:
    changelog = (release.REPO_ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    assert release.release_notes(changelog, Version.parse("0.1.0")).strip()
    # The next release can be cut from it as it stands.
    rolled = release.roll_changelog(changelog, Version.parse("0.2.0"), DATE)
    assert release.release_notes(rolled, Version.parse("0.2.0")).strip()


def test_notes_command_prints_the_section(capsys: pytest.CaptureFixture[str]) -> None:
    assert release.main(["notes", "v0.1.0"]) == 0
    assert "### Added" in capsys.readouterr().out
    assert release.main(["notes", "9.9.9"]) == 1
    assert "no `## [9.9.9]` section" in capsys.readouterr().err
