# Changelog

All notable changes to joust are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and joust uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add each change under **Unreleased** as it lands. `make release VERSION=x.y.z`
turns that section into the release's section, and the release workflow puts
it at the top of the GitHub release notes.

## [Unreleased]

### Added

- LanceDB Enterprise support (#4):
  - One Open dialog for a local folder or a server, using the `lancedb` CLI's
    profiles and credential order. **Test connection** reports what the server
    supports.
  - Server-side SQL, one request per statement, with cancel, **Next page** for
    paged `SHOW` results, a live row count and `EXPLAIN ANALYZE` plans.
  - Functions and computed columns: source, signature, packages and versions,
    freshness counts with **Refresh** and **Errors**, and SQL templates.
  - A Jobs screen (filters, search, progress, events, Pause / Resume / Cancel)
    and Dashboards over the `system.public` metering views.
- `scripts/load_lance.py` loads a Lance dataset (a local `*.lance` directory or
  an object store / `hf://` URI) into a table of a LanceDB database (#3).
- CI on every pull request and push to `main`: rustfmt; clippy and the tests on
  Linux and macOS, with the 90% coverage gate on Linux; cargo-deny; and ruff,
  ty, pytest and the lancedb / pylance pin check for the Python scripts.
- `make release VERSION=x.y.z` bumps the version, rolls this changelog, commits
  and tags. The release workflow fails a tag whose version has no section here.
- Dependabot for Cargo, GitHub Actions and the scripts' uv project, and a
  pre-commit configuration.

### Changed

- lancedb 0.39 → 0.41 and lance 12 → 14 (#4).
- The Python scripts follow: lancedb 0.41 and pylance 14, on Python 3.11 or
  newer.

### Fixed

- Building from source failed after bigdecimal 0.4.11 was removed from
  crates.io; Cargo.lock pins 0.4.10.

## [0.1.0] - 2026-09-28

First release, with prebuilt binaries for Linux (x86-64, ARM64) and macOS
(Apple silicon, Intel).

### Added

- SQL over a local LanceDB database through DataFusion: joins, aggregates,
  window functions, CTEs, `EXPLAIN`, `INSERT INTO`, `CREATE TABLE … AS SELECT`
  and `DROP TABLE`.
- `vector_search` and `fts` table functions, and **Find similar** on a vector
  cell.
- A database explorer with row counts, versions, sizes and indexes, and
  one-click scalar, vector and full-text index builds.
- A SQL editor with syntax highlighting, selection runs, cancel and history.
- A virtualised results grid with sorting, resizing, a cell inspector and CSV
  export.
- Column profiles, a query-plan visualiser and bar, line, scatter and
  histogram charts.
- Images, audio, video and PDFs as first-class values: descriptions, previews,
  a media gallery, video filmstrips, media SQL functions, **Import media…** and
  colour-vector similarity search.
- Nine themes and a sample database.

[Unreleased]: https://github.com/justinrmiller/joust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/justinrmiller/joust/releases/tag/v0.1.0
