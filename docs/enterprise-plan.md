# joust × LanceDB Enterprise — plan

Baseline: the **lancedb 0.41.0** crate (now in `Cargo.toml`) and **Sophon
`main` @ `ebc40b7351`** (server release `1.15.0-beta.0`, 2026-11-24).

## What shapes the design

1. **Run SQL on the server, not in joust.** Joust today wraps each table in a
   DataFusion `BaseTableAdapter` and plans locally. Against a `db://`
   connection, that adapter pulls every row over REST (`k = isize::MAX`) and
   none of the Enterprise SQL dialect is available. So an Enterprise connection
   sends each statement to the server's Flight SQL endpoint with
   `Connection::execute_query_async` (0.41: status, progress, cancel, streamed
   Arrow reader). Local directories keep today's DataFusion path.
2. **SQL is the Enterprise control surface.** On 1.15 the REST routes for
   Functions, views, materialized views and secrets are gone (0 left in
   `phalanx/openapi.yml`); they are SQL only: `CREATE / DESCRIBE / DROP
   FUNCTION`, `SHOW FUNCTIONS [HISTORY]`, `ALTER TABLE … ADD COLUMN c AS f(x)`,
   `REFRESH COLUMN`, `SHOW COLUMN STATUS`, `SHOW ERRORS`, `SHOW JOBS
   [EXTENDED]`, `SHOW JOB EVENTS`, `DESCRIBE JOB`, `SHOW INDEXES / STATS FOR
   TABLE`, `SHOW SERVER VERSION`, `SHOW BUILTIN FUNCTIONS`, `SHOW DATABASES /
   TABLES / VIEWS / MATERIALIZED VIEWS`, and more.
   **lancedb 0.41's `create_function_async`, `list_functions`, `*_view`,
   `*_secret` still call those removed REST routes, so joust must not use
   them.** Its jobs calls (`list_jobs`, `cancel/pause/resume_job`,
   `Job::events`) hit `/v1/jobs/*`, which 1.15 still serves.
3. **There is a system database to build dashboards on.** `system.public.*`
   (read-only, Flight SQL only, cluster-wide) has `job_metering`,
   `service_metering`, `storage_metering`, `gpu_node_metering`,
   `function_refresh_metering` and `query_stats` (a `pg_stat_statements`-style
   view, off by default and superuser-only). Reading needs System Usage plus
   Select on each view. **No existing LanceDB web UI shows query stats,
   metering or cache health,** so these panels would be new for customers.
4. **REST still covers what SQL doesn't.** `/v1/server/info`,
   `/v1/jobs/{stats,timeline,retry,list_children}`,
   `/v1/table/{id}/{stats,errors,function_columns/status,get_rte_stats}` and
   `/v1/cluster/usage` exist. 0.41 wraps the table and job ones. The rest need
   a small raw-HTTP client. The SDK keeps the `phalanx-version` header private,
   so joust reads it itself.
5. **Sign-in can reuse the CLI's profiles.** 0.41's `OAuthFlow` supports
   `ClientCredentials`, `AuthorizationCode` (browser, PKCE) and `DeviceCode`.
   `~/.lancedb/config.toml` (`[profiles.NAME]` with `http_server_url`,
   `sql_server_url`, `database`, `api_key` / `oidc_access_token_file` /
   `[profiles.NAME.oidc]`, TLS cert paths) is a small serde struct joust can
   read.
6. **Local testing is easy.** `lancedb server --data-dir DIR --database joust`
   serves REST on 10024 and Flight SQL on 10025 with no auth. Functions need
   the Linux/Docker server (`make local-server` in Sophon) with
   `derived_compute_enabled` and manifest mode.

## Phases

### Phase 0 — Prove the transport (done 2026-11-24)

The probe is `examples/enterprise_probe.rs` (`cargo run --example
enterprise_probe`). It ran against `lancedb server` built from Sophon `main`
(`1.15.0-beta.0`, `ebc40b7351`) in manifest mode, using lancedb 0.41.0.

**Decision gate: passed.** `Connection::execute_query_async` works against
1.15, so joust uses it. A raw `arrow-flight` client is not needed.

| Check | Result |
|---|---|
| Connect (`db://joust`, host override, API key) | Works. `region` must be set but is unused with a host override. |
| REST: create table, paged table list, stats, indices | Work. |
| SQL: `SELECT` and streaming | 100k rows arrive in 98 batches; first batch after ~90 ms (debug server). |
| SQL: parameters (`$1` bound from an Arrow batch) | Work. |
| SQL: cancel | `Query::cancel` stopped a running 10⁹-row join within ~1 ms; the reader gets "was cancelled" and the server stays healthy. Cancelling a finished query returns "not cancellable". |
| SQL: progress | `describe().progress` stays `None` while a plain query runs. |
| SQL: empty results | Zero batches, but the stream's `schema()` still has the columns. |
| SQL: several statements in one request | Rejected: "only supports a single SQL statement". |
| `SHOW SERVER VERSION / DATABASES / NAMESPACES / TABLES / STATS / INDEXES / JOBS` | Work. `SHOW JOBS` includes progress, phase, rows planned/computed and workers. |
| `SHOW BUILTIN FUNCTIONS` | 429 rows of name, kind, syntax and description: enough for autocomplete. |
| `SHOW FUNCTIONS`, `SHOW MATERIALIZED VIEWS` | Work in manifest mode. Without it the error says "require database_manifest_enabled=true". |
| System views (`job_metering`, `service_metering`, `query_stats`) | Readable. `query_stats` returns no rows, not an error, when the feature is off. |
| `ORDER BY distance(vec, [...]) LIMIT k` | Works (no index needed). |
| `EXPLAIN ANALYZE` | DataFusion's indented text with metrics; `RemotePlanExec` lines carry `workers=N, local_fallback_partitions=M`. |
| `CREATE TABLE AS`, `DROP TABLE` | Work. CTAS runs as a `sql` job (~2.6 s); DROP returns a cleanup job id. |
| `CREATE INDEX … USING btree` | Returns `(result, job_id)`; the job shows in 0.41 `list_jobs` and in `SHOW JOBS`. |
| 0.41 `list_functions` (REST) | 404 `Route not found`, as predicted. Removed routes answer 404 with the server build in the body. |
| `phalanx-version` header | On authenticated `/v1` responses; absent on `/ready`. |
| `/v1/server/info` | Release, git SHA, build date, Lance and lancedb versions. |

What this changes:

- **The statement splitter is required.** The server takes one statement per
  request.
- **The status bar shows elapsed time and rows received.** It shows
  `progress` only when the server reports one.
- **Capability probing reads error text and empty results.** Functions off
  shows up as the manifest error. `query_stats` being off can't be told apart
  from "no queries yet" from SQL alone; that needs another signal (open
  question).
- **Local server for development and tests:** a cold debug build of
  `lancedb-cli` from Sophon `main` takes ~30 min on this Mac and needs access
  to the private forks. Run it with `RUST_MIN_STACK=67108864`; with the default
  2 MiB thread stacks, querying `system.public.service_metering` overflows a
  worker's stack and aborts the server.

### Phase 1 — Connect to Enterprise (done 2026-11-24)

**Status:** built and tested against a local 1.15 server, with 97% line
coverage when that server is available. Differences from the outline below:

- `Database` is an enum (`Local | Remote`) rather than a trait.
- No OS keychain yet: a typed API key lives in memory for the session only.
- Device-code sign-in works only by reusing a `lancedb auth login` session.
  The SDK prints device codes on stderr, which a window can't show.
- `SHOW DATABASES` hides `system` (reachable over SQL only) and internal
  `_` databases from the picker.
- "Find similar" already emits `ORDER BY distance(…)` on a server.

Learned for Phase 2: `SHOW TABLES` and similar statements return one page and
put the next page's token in the result schema's `lancedb.next_page_token`
metadata. joust should offer to continue, as the CLI does.

The original outline:

- Connection picker with two modes: **Local** (today's path box) and
  **Enterprise**. Enterprise fields: REST URL, SQL URL (default: REST port +
  1), database, auth (API key, OIDC token file, OAuth client credentials,
  browser or device login).
- Import profiles from `~/.lancedb/config.toml`, read-only. Credential
  precedence follows the CLI (`LANCEDB_API_KEY`, then token env vars, then the
  profile).
- Keep secrets out of `settings.json`. Recent connections become
  `Local(path) | Enterprise(profile)`.
- Split `Database` into a backend trait with `LocalBackend` (today's
  `db/mod.rs`) and `RemoteBackend`, sharing `TableInfo` and `QueryOutcome`.
- Sidebar tree: database → namespace → tables, views, materialized views,
  using 0.41's paginated `Listing` or `SHOW …` statements. A database switcher
  backed by `SHOW DATABASES`.
- At connect, fill a **`Capabilities`** record by probing, not by matching
  version numbers: server version (`phalanx-version` header or
  `/v1/server/info`), Flight SQL reachable, Functions enabled (`SHOW
  FUNCTIONS`), system access (`LIMIT 0` on `job_metering`), and `query_stats`
  on. Show the server version in the status bar.
- **Browse-only fallback** for when Flight SQL is unreachable (Helm ships
  `disableFlightSql: true`): table list, preview via `table.query()`, stats and
  jobs over REST, with a banner explaining that SQL needs Flight enabled.

### Phase 2 — Server-side SQL editor (done 2026-11-24)

**Status:** built and tested against the local 1.15 server.
- The editor's statements are split client-side, skipping quotes, comments
  and `$$` / `$tag$` bodies. A `$` straight after a name (`team$reports`) or
  before a digit (`$1`) doesn't start a dollar quote.
- The statements run one request each, and the last result shows. A failure
  says which statement failed ("statement 2 of 3 failed").
- **Cancel** aborts locally and also sends `Query::cancel` to the server.
- While a query runs, the status bar shows rows received.
- A paged `SHOW` gets a **Next page** button: the same statement with
  `LIMIT 1000 FROM '<token>'`.
- **Collect plan** (off by default) runs `EXPLAIN ANALYZE` on the last query
  and parses the text into the plan diagram.
- Enterprise statement keywords are highlighted.

Not done: autocomplete. joust's editor has no completion yet, so
`SHOW BUILTIN FUNCTIONS` isn't used. "Find similar" emits `distance()`
(Phase 1), and the index buttons use REST `create_index`, which the server
also runs as a job.

The original outline:


- Remote `run_sql`: split the editor text into statements client-side, then
  run each with `execute_query_async` and stream up to the row limit. The
  splitter must handle quotes, comments and `$$` bodies; DataFusion's parser
  can't parse Enterprise syntax like `CREATE FUNCTION`. **Cancel** calls
  `Query::cancel` instead of only aborting the local task (Phase 0: it stops a
  running query within milliseconds). The status bar shows elapsed time and
  rows received, plus `describe().progress` when the server reports one.
- **Plan tab:** run `EXPLAIN ANALYZE` on the server as an opt-in toggle,
  because it re-executes the query. Parse the indented DataFusion text into the
  existing `PlanNode` tree (`db/plan.rs` gains a text parser), and show
  `workers=N, local_fallback_partitions=M` on `RemotePlanExec` nodes.
- **Server dialect:** "Find similar" emits `ORDER BY distance(col, [...])
  LIMIT k`, which the server rewrites to ANN, instead of joust's local
  `vector_search(...)`. The index buttons emit `CREATE INDEX … USING …`, which
  returns a job id.
- Add Enterprise keywords to highlighting, and fill autocomplete from `SHOW
  BUILTIN FUNCTIONS`.
- The Rows, Columns, Media and Chart tabs work unchanged on the returned
  batches.

### Phase 3 — Functions and computed columns (done 2026-11-24)

**Status:** built and tested end to end; Functions run on this Mac's local
server.
- **Sidebar:** a FUNCTIONS section lists the newest version of each Function,
  from paged `SHOW FUNCTIONS HISTORY`.
- **Function tab:** opened by selecting a Function. It shows the runtime,
  signature, packages, image and source (`DESCRIBE FUNCTION … WITH
  (version, include_source)`), every version, and the Function columns of
  expanded tables.
- **Function actions:**
  - **New version…** opens `CREATE OR REPLACE FUNCTION` with the current
    source.
  - **Drop…** opens `DROP FUNCTION … WITH (version = …)`. It is never run for
    you.
- **Computed columns** come from field metadata: `computed_column.kind` and
  `computed_column.expression`. They're badged ƒ (Function) or = (expression,
  shown in a tooltip).
- **Column freshness:** expanding a table with Function columns loads
  `SHOW COLUMN STATUS` and shows fresh / stale / failed / never-computed
  counts, with **Refresh** (`REFRESH COLUMN`) and **Errors** (`SHOW ERRORS`).
- **Templates:** **+ Column** writes an `ALTER TABLE … ADD COLUMN … AS`
  template, and **+ New** a `CREATE FUNCTION` template. Materialized views
  get **Refresh**.
- **Bound columns:** a server-wide list needs a `SHOW COLUMN STATUS` per
  table, so the Function tab lists only expanded tables' columns.
- Learned: a server refuses to overwrite a table that has Function columns.

The original outline:


- **Sidebar "Functions" section** from `SHOW FUNCTIONS` (name, version, Python
  version, signature, packages). Selecting one runs `DESCRIBE FUNCTION f WITH
  (include_source = true)` and shows the source read-only, with version history
  from `SHOW FUNCTIONS HISTORY`.
- **New Function template:** inserts a `CREATE FUNCTION … LANGUAGE python AS
  $$ … $$ WITH (pip = [...])` skeleton. The returned job id opens in the Jobs
  panel.
- **Column badges** on computed and Function columns, with freshness counts
  from `SHOW COLUMN STATUS ON t` (fresh, stale, never computed, failed). Each
  column gets **Refresh** (`REFRESH COLUMN … WITH (error_policy = …)`) and
  **Show errors** (`SHOW ERRORS ON t` into the grid).
- An **Add computed column** dialog that generates `ALTER TABLE t ADD COLUMN c
  AS <expr | f(col) WITH (version = '…')>`.
- **Materialized views:** list, describe, refresh, drop, all in SQL. 0.41's
  materialized-view helpers already send SQL that 1.15 accepts.

### Phase 4 — Jobs panel (done 2026-11-24)

**Status:** built and tested against the local server and a test deployment
(about 320 jobs).
- **Top bar:** on a server, **SQL / Jobs / Dashboards** switch screens, and
  the **Database** picker (moved out of the sidebar) applies to all three.
  Jobs shows "Jobs · N active" while any job runs.
- **List:** `SHOW JOBS EXTENDED`, newest first and capped at 300. Filters are
  All / Active / Failed / Finished, with counts, plus search by id, type,
  table or Function. Each row shows a state pill (dot and word), the type,
  table, Function, created time, progress bar, workers, phase or queue reason
  and duration. It polls every 3 s while the screen shows.
- **Detail:** facts from the row plus `DESCRIBE JOB … FORMAT JSON` (tasks,
  workers, the per-phase timeline) and `SHOW JOB EVENTS` (newest 50).
  - A job with no profile yet (not started) shows its events alone.
  - A running job's profile can have an empty `state`; joust falls back to
    `job_state`, then the newest event.
- **Actions:** **Pause** / **Resume**, and **Cancel…**, which needs a second
  click ("Confirm cancel"). The result shows in the detail panel.
  - `cancel_job` returns true when the server accepted the request and false
    for an unknown job. It does not report "already finished".
  - Job ids must look like the server's (`j1-<kind>-<9 base36>-…`), or the
    server answers 400.
- **Open job:** a result with a `job_id` column (CREATE INDEX, OPTIMIZE,
  refreshes) gets **Open job** in the results header.
- **Not done:** retry (`/v1/jobs/retry`) and the Chrome-trace export.

The original outline:

- A **Jobs** view backed by `SHOW JOBS EXTENDED`: state, phase, progress, rows
  planned / computed / failed, workers, queue reason, failure message. It polls
  on a subscription tick only while visible.
- **Actions:** cancel, pause, resume (0.41 calls) and retry (REST
  `/v1/jobs/retry`).
- **Job detail:** an event timeline from `SHOW JOB EVENTS 'id'`, `DESCRIBE JOB
  'id' FORMAT JSON`, and optionally the Chrome-trace export from
  `/v1/jobs/timeline`.
- Jobs that joust itself starts (index builds, refreshes, Function creation)
  open here automatically.

### Phase 5 — Dashboards (done 2026-11-24)

**Status:** built and tested against the local server and a test deployment.
- **Tiles** (`enterprise/dashboard.rs`, data only):
  - Headline numbers: tables (catalog), jobs finished and failed
    (`job_metering`), active jobs (`SHOW JOBS EXTENDED`).
  - Stacked bars: rows refreshed per Function (`function_refresh_metering`)
    and service CPU-hours (`service_metering`).
  - Tables: table health (`SHOW STATS` / `SHOW INDEXES FOR TABLE`, at most
    100 tables), storage by table (`storage_metering`, latest sample), failed
    jobs, and slowest statements (`query_stats`).
  - Slowest statements comes last: it needs `query_stats.enabled` on the
    query node.
- **Scope:** the metering views cover the whole cluster. Jobs, refreshes,
  storage and failed jobs are filtered to the open database
  (`database_name`). Service CPU and slowest statements are cluster-wide and
  say so.
- **Ranges:** last 24 hours (24 hourly bars), 7 or 30 days (daily bars). Every
  bucket gets a bar, so days with no data show as gaps. Refreshes every 60 s
  while shown, skipping a tick while the last refresh is still loading.
  Table health (two statements per table) is re-checked at most every
  10 minutes by the timer. Late replies from an older refresh, or from a
  database that was switched away from, are dropped.
- **Chart** (`ui/series_chart.rs`):
  - Stacked bars in the validated categorical palette, with separate light
    and dark steps.
  - Colour follows the series: the five largest are kept, listed
    alphabetically, and the rest fold into "other".
  - 2px gaps between segments and a rounded top.
  - The legend gives each series' total; hovering a bar shows its breakdown.
- **Errors:** a tile shows the server's message (the full error on hover). The
  Jobs and Dashboards screens no longer show the SQL screen's banner.
- **Open in editor** runs a tile's SQL (for the current range) in the SQL
  screen. Table health has no single query: it lists every table it checked,
  and clicking one (or **Open in editor**, for the first) runs
  `SHOW STATS FOR TABLE` for it.
- **Left out:**
  - Compute (CPU/GPU-hour) tiles. On 1.15.0-beta.0 servers with metering data,
    reading `job_metering.total_cpu_millis` / `total_gpu_millis` fails with
    "validity read requires a filtered read or empty variant". The cause is
    `UseValidityRead` (#9720) meeting the distributed codec. Filed as
    ENT-4352.
  - GPU held vs used, live WAL lag, Prometheus, and user-defined tiles.

The original outline:

A dashboard is a grid of **tiles**. Each tile is a SQL query plus a chart
spec, refreshed on a timer and drawn with the existing `ui/chart.rs` and grid.
Tiles are stored as data, so users can add their own SQL tiles later.

| Tile | Source | Who can see it |
|---|---|---|
| Table health: rows, size, fragments, small fragments (compaction backlog), unindexed rows per index, versions | `SHOW STATS / INDEXES FOR TABLE` | any user |
| Function columns: freshness, refresh duration, p95 split latency, rows/s, workers | `SHOW COLUMN STATUS`, `system.public.function_refresh_metering` | System Usage |
| Jobs and cost: CPU/GPU/memory-hours per day by job type and table, failures | `job_metering` | System Usage |
| Service and storage usage over time; storage growth per table | `service_metering`, `storage_metering` | System Usage |
| Query performance: top statements by total time, mean / stddev, rows scanned, bytes read | `query_stats` | superuser, `query_stats.enabled` |
| GPU held vs used | `gpu_node_metering`, `/v1/cluster/usage` | System Usage |
| Live WAL tables: memtable and SSTable lag | `Table::get_rte_stats` | table access |

- **Chart work needed:** a series ("group by") column, time bucketing, and
  stacked bar / multi-line charts. `chart.rs` currently draws a single X/Y
  series.
- **Clear empty states** that tell apart permission denied, feature turned off
  and server too old (from `Capabilities`).
- **Later:** scrape Prometheus `/metrics` (served on the main port by default)
  for live QPS, latency and cache hit rate. It is unauthenticated, and whether
  it's reachable through customer gateways is unverified.

### Phase 6 — Admin (optional, read-only first)

- Who am I (0.41 `authz` API), `SHOW GRANTS` / `SHOW ROLE BINDINGS`, the
  secrets list (names only, never values), and cache status.

## Code layout

- `src/db/mod.rs` becomes a backend trait, implemented in `src/db/local.rs`
  (today's code) and `src/db/remote.rs`, plus `src/db/statements.rs` (the SQL
  splitter) and `src/db/capabilities.rs`.
- `src/enterprise/{profiles,functions,jobs,dashboards,http}.rs` hold profile
  parsing, the SQL builders and parsers, and a raw REST client. `reqwest` is
  already transitive via `remote`; pin the same version as a direct dependency.
- `src/ui/{connect,functions,jobs,dashboard}.rs`, plus new `ResultTab` and
  sidebar sections, and new `Message` variants in `src/app.rs`.

## Testing

- Keep the 90% coverage gate. Put SQL execution behind the backend trait so
  view and logic tests use a fake backend that returns canned `RecordBatch`es.
- Parse and builder code (profiles, splitter, `EXPLAIN` text, `SHOW` result
  mapping) is pure and unit-tested.
- Integration tests are behind a feature or `#[ignore]` and spawn the
  `lancedb server` binary named by `JOUST_LANCEDB_BIN` (pattern:
  `lancedb-cli-integtest`). Functions tests run against the Docker server,
  manually or nightly.

## Risks and open questions

1. **SDK/server skew.** 0.41's Function, view and secret calls target REST
   routes 1.15 removed, so we go through SQL. Revisit when a lancedb release
   matches 1.15 (Sophon `main` builds against a private lancedb fork).
2. **Older servers.** 1.9–1.14 lack many `SHOW` statements and system views.
   Probing plus graceful degradation covers this, but we need a stated minimum.
3. **Flight SQL is off by default in Helm.** Customers must enable it for SQL
   mode; the browse-only fallback covers the rest.
4. **Statement splitting with Enterprise syntax.** Phase 0 confirmed the
   server rejects several statements in one request, so joust splits them
   itself. The splitter must handle quotes, comments and `$$` bodies.
5. **`EXPLAIN ANALYZE` re-executes the query,** so it stays opt-in.
6. **Local dev on macOS.** Building `lancedb-cli` from Sophon `main` needs the
   private `lancedb-internal-fork`. Prebuilt macOS CLIs exist only for older
   releases. Functions need Linux.

## Decisions needed

- **Minimum server version** to support (suggest 1.13+: `SHOW SERVER VERSION`,
  `/v1/server/info`, `SHOW COLUMN STATUS`, `query_stats`).
- **Where API keys live:** env and CLI profile only, or also the OS keychain
  (a new `keyring` dependency).
- **Dashboards:** a fixed set first, or user-defined SQL tiles from the start.

## Suggested milestones

- **M1 — Phases 0–2:** connect, server-side SQL and browsing. Most of the
  value.
- **M2 — Phases 3–4:** Functions and Jobs.
- **M3 — Phase 5:** Dashboards.
- **M4 — Phase 6:** Admin.
