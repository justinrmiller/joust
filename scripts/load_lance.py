#!/usr/bin/env -S uv run
"""Load a Lance dataset into a table of a LanceDB database.

The rows are streamed in batches, so the dataset doesn't need to fit in memory.
Only the data is copied: the new table starts at version 1 without the source's
history or indexes (build indexes from joust's sidebar afterwards).

Usage:
    scripts/load_lance.py SOURCE DATABASE [--table NAME] [--mode MODE]

SOURCE is a Lance dataset (a ``*.lance`` directory, or an object store URI) and
DATABASE a LanceDB database directory or URI, created if missing. The table is
named after SOURCE unless ``--table`` is given. ``--mode`` decides what happens
when the table already exists: ``create`` (the default) refuses, ``overwrite``
replaces it and ``append`` adds the rows to it (the schemas must match).

The script needs uv (https://docs.astral.sh/uv/). Its dependencies are in the
repository's pyproject.toml (pinned in uv.lock), and uv installs them into
``.venv`` on first run; run it from anywhere inside the repository, or with
``uv run --project /path/to/joust scripts/load_lance.py …`` from outside.
"""

from __future__ import annotations

import argparse
import sys
import time
from collections.abc import Iterator
from pathlib import Path

import lance
import lancedb
import pyarrow as pa


def default_table_name(source: str) -> str:
    """``/data/movies.lance`` → ``movies``."""
    name = source.rstrip("/").rsplit("/", 1)[-1]
    return name.removesuffix(".lance") or name


def same_local_path(a: str, b: str) -> bool:
    """Whether two URIs are the same local directory (never for object stores)."""
    if "://" in a or "://" in b:
        return False
    return Path(a).resolve() == Path(b).resolve()


def has_table(db: lancedb.DBConnection, name: str) -> bool:
    """Whether the database has a table called ``name``."""
    token = None
    while True:
        page = db.list_tables(page_token=token)
        if name in page.tables:
            return True
        token = page.page_token
        if not token:
            return False


def counted(reader: pa.RecordBatchReader, total: int) -> Iterator[pa.RecordBatch]:
    """Yield the reader's batches, printing progress to stderr."""
    copied = 0
    for batch in reader:
        copied += batch.num_rows
        print(f"\r  {copied:,} / {total:,} rows", end="", file=sys.stderr, flush=True)
        yield batch
    print(file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Load a Lance dataset into a LanceDB table.",
    )
    parser.add_argument(
        "source", help="Lance dataset path or URI (e.g. data/movies.lance)"
    )
    parser.add_argument("database", help="LanceDB database directory or URI")
    parser.add_argument("--table", help="table name (default: the dataset's name)")
    parser.add_argument(
        "--mode",
        choices=["create", "overwrite", "append"],
        default="create",
        help="if the table exists: fail (create), replace it, or append to it",
    )
    parser.add_argument(
        "--version",
        type=int,
        help="load this version of the dataset instead of the latest",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=8192,
        help="rows per batch read from the dataset (default: %(default)s)",
    )
    args = parser.parse_args(argv)

    table = args.table or default_table_name(args.source)
    db = lancedb.connect(args.database)
    target = f"{args.database.rstrip('/')}/{table}.lance"
    if args.mode != "append" and same_local_path(args.source, target):
        parser.error(f"{args.source} is already table {table!r} of {args.database}")
    exists = has_table(db, table)
    if args.mode == "append" and not exists:
        parser.error(f"--mode append: {args.database} has no table {table!r}")
    if args.mode == "create" and exists:
        parser.error(
            f"{args.database} already has a table {table!r} "
            "(use --mode overwrite or --mode append, or --table another name)"
        )

    if "://" not in args.source and not Path(args.source).is_dir():
        parser.error(f"{args.source}: no such directory")
    try:
        dataset = lance.dataset(args.source, version=args.version)
    except (OSError, ValueError) as err:
        parser.error(f"can't open {args.source} as a Lance dataset: {err}")

    total = dataset.count_rows()
    print(
        f"{args.source} (version {dataset.version}, {total:,} rows) "
        f"-> {args.database} table {table!r}",
        file=sys.stderr,
    )
    reader = dataset.scanner(batch_size=args.batch_size).to_reader()
    batches = pa.RecordBatchReader.from_batches(reader.schema, counted(reader, total))

    started = time.monotonic()
    if args.mode == "append":
        result = db.open_table(table)
        result.add(batches)
    else:
        result = db.create_table(table, batches, schema=reader.schema, mode=args.mode)

    print(
        f"Done in {time.monotonic() - started:.1f}s: {table!r} now has "
        f"{result.count_rows():,} rows (version {result.version}).",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
