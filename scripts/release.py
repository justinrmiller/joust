#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Prepare a joust release, or print a release's notes from CHANGELOG.md.

Usage:
    scripts/release.py prepare VERSION [--dry-run] [--allow-branch]
    scripts/release.py notes VERSION

``prepare`` (``make release VERSION=x.y.z``) checks that the working tree is
clean, that you're on an up-to-date ``main`` and that the tag is new, then:

1. sets ``version`` in Cargo.toml and joust's entry in Cargo.lock;
2. turns CHANGELOG.md's ``## [Unreleased]`` section into
   ``## [VERSION] - YYYY-MM-DD`` (it must not be empty), leaving a new, empty
   Unreleased section above it, and updates the comparison links at the end;
3. commits those three files as "Release joust VERSION" and creates the
   annotated tag ``vVERSION``.

It doesn't push: it prints the ``git push`` that publishes the release. With
``--dry-run`` it only runs the checks and prints the changes it would make.

``notes`` prints the CHANGELOG.md section for VERSION. The release workflow
uses it as the start of the GitHub release notes, and fails a tag whose
version has no section.
"""

from __future__ import annotations

import argparse
import datetime
import difflib
import functools
import itertools
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# The repository root (this file is scripts/release.py).
REPO_ROOT = Path(__file__).resolve().parent.parent

# SemVer 2.0 without build metadata: `1.2.3` or `1.2.3-rc.1`.
VERSION_PATTERN = re.compile(
    r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?"
)

UNRELEASED_HEADING = "## [Unreleased]"
# A level-2 heading starts each release's section.
SECTION_HEADING = re.compile(r"^## ", re.MULTILINE)
# A Markdown link reference definition, e.g. `[0.1.0]: https://…`.
LINK_DEFINITION = re.compile(r"^\[[^\]]+\]:\s")
# `[Unreleased]: <base>/compare/<previous tag>...HEAD`.
UNRELEASED_LINK = re.compile(
    r"^\[Unreleased\]:\s*(?P<base>\S+)/compare/(?P<previous>\S+?)\.\.\.HEAD\s*$",
    re.MULTILINE,
)


class ReleaseError(Exception):
    """A problem that stops the release, with a message for the user."""


@functools.total_ordering
@dataclass(frozen=True)
class Version:
    """A parsed SemVer version, compared by SemVer precedence."""

    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()

    @classmethod
    def parse(cls, text: str) -> Version:
        """Parses ``1.2.3`` or ``1.2.3-rc.1``; raises ``ReleaseError`` otherwise."""
        match = VERSION_PATTERN.fullmatch(text)
        if match is None:
            raise ReleaseError(
                f"{text!r} isn't a version like 1.2.3 or 1.2.3-rc.1 "
                "(SemVer, without build metadata)."
            )
        major, minor, patch, prerelease = match.groups()
        return cls(
            int(major),
            int(minor),
            int(patch),
            tuple(prerelease.split(".")) if prerelease else (),
        )

    def __str__(self) -> str:
        core = f"{self.major}.{self.minor}.{self.patch}"
        return f"{core}-{'.'.join(self.prerelease)}" if self.prerelease else core

    def _key(self) -> tuple[tuple[int, int, int], int, list[tuple[int, int | str]]]:
        """A sort key implementing SemVer precedence (spec item 11)."""
        # A pre-release sorts before the release; numeric identifiers sort
        # numerically and before alphanumeric ones; a shorter list of equal
        # identifiers sorts first (which list comparison does already).
        identifiers: list[tuple[int, int | str]] = [
            (0, int(part)) if part.isdigit() else (1, part) for part in self.prerelease
        ]
        return (
            (self.major, self.minor, self.patch),
            0 if self.prerelease else 1,
            identifiers,
        )

    def __lt__(self, other: Version) -> bool:
        return self._key() < other._key()


def bump_manifest(manifest: str, new: Version) -> tuple[str, Version]:
    """Sets ``version`` in Cargo.toml's ``[package]`` table.

    Returns the new text and the version it replaced.
    """
    package = re.search(r"^\[package\]\s*$", manifest, re.MULTILINE)
    if package is None:
        raise ReleaseError("Cargo.toml has no [package] table.")
    # The table runs until the next table header.
    next_table = re.compile(r"^\[", re.MULTILINE).search(manifest, package.end())
    end = next_table.start() if next_table else len(manifest)
    version_line = re.compile(r'^version\s*=\s*"([^"]+)"', re.MULTILINE)
    found = version_line.search(manifest, package.end(), end)
    if found is None:
        raise ReleaseError("Cargo.toml's [package] table has no version.")
    old = Version.parse(found.group(1))
    text = f'{manifest[: found.start()]}version = "{new}"{manifest[found.end() :]}'
    return text, old


def bump_lock(lock: str, old: Version, new: Version) -> str:
    """Sets the version of joust's own entry in Cargo.lock."""
    entry = re.compile(r'(\[\[package\]\]\nname = "joust"\nversion = ")([^"]+)(")')
    matches = list(entry.finditer(lock))
    if len(matches) != 1:
        raise ReleaseError(
            f"Expected one joust entry in Cargo.lock, found {len(matches)}."
        )
    (match,) = matches
    if match.group(2) != str(old):
        raise ReleaseError(
            f"Cargo.lock has joust {match.group(2)} but Cargo.toml has {old}; "
            "run `cargo check` to sync them first."
        )
    return f"{lock[: match.start(2)]}{new}{lock[match.end(2) :]}"


def split_sections(changelog: str) -> tuple[str, list[str]]:
    """Splits CHANGELOG.md into the text before the first ``## `` heading and
    one string per ``## `` section (heading included)."""
    starts = [match.start() for match in SECTION_HEADING.finditer(changelog)]
    if not starts:
        return changelog, []
    bounds = [*starts, len(changelog)]
    return changelog[: starts[0]], [
        changelog[a:b] for a, b in itertools.pairwise(bounds)
    ]


def without_link_definitions(section: str) -> str:
    """Drops the link reference definitions that trail the last section."""
    lines = section.rstrip("\n").split("\n")
    while lines and (not lines[-1].strip() or LINK_DEFINITION.match(lines[-1])):
        lines.pop()
    return "\n".join(lines)


def roll_changelog(changelog: str, new: Version, date: datetime.date) -> str:
    """Moves the Unreleased entries into a new ``## [VERSION] - DATE`` section."""
    preamble, sections = split_sections(changelog)
    headings = [section.partition("\n")[0].strip() for section in sections]
    if any(heading.startswith(f"## [{new}]") for heading in headings):
        raise ReleaseError(f"CHANGELOG.md already has a section for {new}.")
    if UNRELEASED_HEADING not in headings:
        raise ReleaseError(f"CHANGELOG.md has no `{UNRELEASED_HEADING}` section.")
    index = headings.index(UNRELEASED_HEADING)
    body = sections[index].partition("\n")[2]
    if not without_link_definitions(body).strip():
        raise ReleaseError(
            "CHANGELOG.md's Unreleased section is empty: "
            "list the release's changes first."
        )
    sections[index] = f"{UNRELEASED_HEADING}\n\n## [{new}] - {date.isoformat()}\n{body}"
    text = preamble + "".join(sections)

    link = UNRELEASED_LINK.search(text)
    if link is not None:
        base, previous = link.group("base"), link.group("previous")
        text = (
            f"{text[: link.start()]}[Unreleased]: {base}/compare/v{new}...HEAD\n"
            f"[{new}]: {base}/compare/{previous}...v{new}{text[link.end() :]}"
        )
    return text


def release_notes(changelog: str, version: Version) -> str:
    """The body of CHANGELOG.md's section for ``version``."""
    _, sections = split_sections(changelog)
    for section in sections:
        heading, _, body = section.partition("\n")
        if re.fullmatch(
            rf"## \[{re.escape(str(version))}\](?: - .*)?", heading.strip()
        ):
            notes = without_link_definitions(body).strip("\n")
            if not notes.strip():
                raise ReleaseError(f"CHANGELOG.md's section for {version} is empty.")
            return notes + "\n"
    raise ReleaseError(f"CHANGELOG.md has no `## [{version}]` section.")


def git(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Runs git in the repository and returns the finished process."""
    return subprocess.run(
        ["git", *args], cwd=REPO_ROOT, check=check, capture_output=True, text=True
    )


def check_repository(tag: str, allow_branch: bool) -> None:
    """Refuses to release from a dirty tree, a stale or other branch, or onto an
    existing tag."""
    if git("status", "--porcelain", "--untracked-files=no").stdout.strip():
        raise ReleaseError(
            "The working tree has uncommitted changes; commit or stash them."
        )
    if not allow_branch:
        branch = git("branch", "--show-current").stdout.strip()
        if branch != "main":
            raise ReleaseError(
                f"You're on {branch or 'a detached HEAD'}, not main "
                "(--allow-branch overrides this)."
            )
        fetched = git("fetch", "--quiet", "origin", "main", check=False)
        if fetched.returncode != 0:
            raise ReleaseError(f"Couldn't fetch origin/main: {fetched.stderr.strip()}")
        head = git("rev-parse", "HEAD").stdout.strip()
        upstream = git("rev-parse", "FETCH_HEAD").stdout.strip()
        if head != upstream:
            raise ReleaseError(
                "main isn't the same commit as origin/main; pull or push first."
            )
    local = git("rev-parse", "--quiet", "--verify", f"refs/tags/{tag}", check=False)
    if local.returncode == 0:
        raise ReleaseError(f"The tag {tag} already exists locally.")
    remote = git(
        "ls-remote", "--exit-code", "--tags", "origin", f"refs/tags/{tag}", check=False
    )
    if remote.returncode == 0:
        raise ReleaseError(f"The tag {tag} already exists on origin.")
    if remote.returncode != 2:
        raise ReleaseError(f"Couldn't list origin's tags: {remote.stderr.strip()}")


def show_diff(path: str, before: str, after: str) -> None:
    """Prints a unified diff of one file's planned change."""
    sys.stdout.writelines(
        difflib.unified_diff(
            before.splitlines(keepends=True),
            after.splitlines(keepends=True),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
        )
    )


def prepare(version_text: str, dry_run: bool, allow_branch: bool) -> None:
    """Bumps the version, rolls the changelog, commits and tags."""
    new = Version.parse(version_text)
    tag = f"v{new}"
    check_repository(tag, allow_branch)

    paths = {
        name: REPO_ROOT / name for name in ("Cargo.toml", "Cargo.lock", "CHANGELOG.md")
    }
    before = {name: path.read_text(encoding="utf-8") for name, path in paths.items()}
    manifest, old = bump_manifest(before["Cargo.toml"], new)
    if not new > old:
        raise ReleaseError(f"{new} isn't newer than the current version, {old}.")
    after = {
        "Cargo.toml": manifest,
        "Cargo.lock": bump_lock(before["Cargo.lock"], old, new),
        "CHANGELOG.md": roll_changelog(
            before["CHANGELOG.md"], new, datetime.date.today()
        ),
    }

    if dry_run:
        for name in paths:
            show_diff(name, before[name], after[name])
        print(f"\nDry run: would commit these changes and tag {tag}.")
        return

    for name, path in paths.items():
        path.write_text(after[name], encoding="utf-8")
    # Cargo would rewrite Cargo.lock if the edit above left it inconsistent.
    metadata = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if metadata.returncode != 0:
        for name, path in paths.items():
            path.write_text(before[name], encoding="utf-8")
        raise ReleaseError(
            f"Cargo.lock didn't check out; nothing changed:\n{metadata.stderr}"
        )
    git("add", "--", *paths)
    git("commit", "--quiet", "-m", f"Release joust {new}")
    git("tag", "--annotate", tag, "-m", f"joust {new}")
    print(f"Committed and tagged {tag} ({old} -> {new}).")
    print("To publish the release, push the commit and the tag together:\n")
    print(f"    git push --atomic origin HEAD {tag}")


def main(argv: list[str] | None = None) -> int:
    """Parses the command line and runs a command; returns the exit status."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser(
        "prepare", help="bump, roll the changelog, commit, tag"
    )
    prepare_parser.add_argument(
        "version", help="the new version, e.g. 0.2.0 or 0.2.0-rc.1"
    )
    prepare_parser.add_argument(
        "--dry-run", action="store_true", help="check and show the changes only"
    )
    prepare_parser.add_argument(
        "--allow-branch",
        action="store_true",
        help="release from a branch other than main",
    )
    notes_parser = commands.add_parser(
        "notes", help="print a release's CHANGELOG.md section"
    )
    notes_parser.add_argument("version", help="e.g. 0.2.0; a leading v is ignored")
    args = parser.parse_args(argv)

    try:
        if args.command == "prepare":
            prepare(args.version, args.dry_run, args.allow_branch)
        else:
            changelog = (REPO_ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
            version = Version.parse(args.version.removeprefix("v"))
            sys.stdout.write(release_notes(changelog, version))
    except (ReleaseError, OSError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", None)
        print(
            f"release.py: {error}{f' {detail.strip()}' if detail else ''}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
