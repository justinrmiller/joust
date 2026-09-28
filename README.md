# joust

A desktop SQL workbench for [LanceDB](https://lancedb.com), built with
[iced](https://iced.rs): a database explorer on the left, a SQL editor on top,
and results below with a data grid, a column profiler, a media gallery, a
query-plan visualiser and charts.

(A *lance* is what you *joust* with.)

![Results grid with an image column and the cell inspector's preview](docs/themes/joust-light.png)

## Features

- **SQL over LanceDB.** Every table in the database is registered with
  [DataFusion](https://datafusion.apache.org), so joins, aggregates, window
  functions, CTEs, `EXPLAIN`, `information_schema` and `INSERT INTO` all work.
  `CREATE [OR REPLACE] TABLE … AS SELECT` materialises a real Lance table and
  `DROP TABLE` drops one.
- **Vector and full-text search from SQL.**
  - `vector_search('table', 'column', '[0.1, 0.2, …]', k [, 'l2'|'cosine'|'dot'])`
    runs a LanceDB nearest-neighbour query and adds a `_distance` column.
  - `fts('table', '{"match": {"column": "title", "terms": "die hard"}}')` runs a
    full-text query (needs an FTS index; there is a one-click button for it).
  - Select any vector cell in the results and press **Find similar** to search
    for its neighbours.
- **Database explorer.** Tables with row counts, version, size and indexes;
  expand to see columns and types, click a column to insert it into the editor,
  one-click **Preview**, and (on hover) buttons to build a scalar, vector or
  full-text index on a column.
- **SQL editor** with theme-aware syntax highlighting. `Ctrl+Enter` (or `F5`)
  runs the whole editor, or only the selection; multiple statements run in
  order and the last result set is shown. Long queries can be cancelled with
  `Esc` or the Cancel button, which stops the query itself, not just the wait.
  Queries run on their own thread pool (leaving a core free), so the UI stays
  responsive while DataFusion is busy. Query history is kept in the sidebar.
- **Results grid.** Virtualised canvas grid that stays smooth with hundreds of
  thousands of rows: click headers to sort (asc → desc → off), drag header
  edges to resize, click or use the arrow keys / PageUp / PageDown to move the
  selection, `Ctrl+C` to copy a value. A cell inspector shows the full value.
  Results can be exported to CSV.
- **Column explorer.** Per-column profile cards: null rate, distinct count,
  histograms for numbers, timestamps and vector norms, top values for text, and
  format breakdowns, byte sizes and image dimensions for media columns.
- **Multimodal data.** LanceDB tables often hold images, audio, video or PDFs
  next to their embeddings; joust treats them as first-class values:
  - Binary cells are recognised by their bytes and described instead of
    hex-dumped (`PNG 120×180 · 3.8 KB`, `WAV audio · 1.2 MB`). Text columns
    holding paths to local image files are recognised too.
  - The cell inspector previews images, and **Save as…** writes any binary
    value to a file.
  - A **Media** tab shows a thumbnail gallery whenever a result has an image
    column (thumbnails are decoded off the UI thread).
  - **Find similar images** computes the selected image's colour vector and
    runs `vector_search` against the table's colour-vector column.
  - SQL functions: `media_type(bytes)` (MIME type), `image_width(bytes)`,
    `image_height(bytes)` and `byte_length(bytes)`.
  - **Import media…** loads a folder (recursively) of images, audio, video and
    PDFs into a new table with `path`, `file_name`, `kind`, `mime`,
    `size_bytes`, `width`, `height`, `modified`, a 256px PNG `thumbnail`, a
    16-d `color_vector` and the original bytes in `data`. Browse the
    `thumbnail` column; `SELECT *` also loads every original.

  The colour vector is a hue/lightness histogram, so "similar" means similar
  palette, not similar content. For semantic search, store embeddings from a
  model of your choice in a vector column and use `vector_search` on it.
- **Query plan visualiser.** After every query the executed physical plan is
  drawn as a tree: each operator shows its parameters, rows produced and
  compute time, with a bar for its share of the slowest operator, and edges are
  labelled with the rows flowing between operators. Hover for every metric;
  drag to pan, `Ctrl`+scroll to zoom. Text views of the physical (with metrics)
  and logical plans are one click away.
- **Charts.** Bar, line, scatter and histogram views of the current result
  with sensible defaults (time column → line chart, category → bar chart),
  hover tooltips and clean axes.
- **Nine themes:** Joust Light, Joust Dark, Lance Midnight, Nord, Dracula,
  Solarized Light, Gruvbox Dark, Tokyo Night and Catppuccin Mocha (see
  [below](#themes)). Each has its own syntax colours. The theme, recent databases, row limit and history
  persist in `~/.config/joust/settings.json` (platform equivalent elsewhere).
- **Sample database.** One click creates `movies` (real titles, years and
  directors with a *synthetic* 8-dimensional genre/era embedding, an FTS index
  on `title`, and a procedurally generated `poster` PNG per film with its
  `poster_colors` vector; the posters are abstract genre-palette art, not the
  real posters) and `events` (50,000 generated analytics events).

## Screenshots

| Media gallery (Joust Dark) | Query plan (Tokyo Night) |
|---|---|
| ![Poster gallery with a selected image](docs/gallery.png) | ![Plan diagram](docs/plan.png) |

| Chart (Lance Midnight) | Column explorer (Nord) |
|---|---|
| ![Line chart](docs/chart.png) | ![Column profiles including a media column](docs/columns.png) |

### Themes

The same view in every theme:

| Joust Light | Joust Dark | Lance Midnight |
|---|---|---|
| ![Joust Light](docs/themes/joust-light.png) | ![Joust Dark](docs/themes/joust-dark.png) | ![Lance Midnight](docs/themes/lance-midnight.png) |
| **Nord** | **Dracula** | **Solarized Light** |
| ![Nord](docs/themes/nord.png) | ![Dracula](docs/themes/dracula.png) | ![Solarized Light](docs/themes/solarized-light.png) |
| **Gruvbox Dark** | **Tokyo Night** | **Catppuccin Mocha** |
| ![Gruvbox Dark](docs/themes/gruvbox-dark.png) | ![Tokyo Night](docs/themes/tokyo-night.png) | ![Catppuccin Mocha](docs/themes/catppuccin-mocha.png) |

## Building

Requirements:

- Rust 1.91 or newer.
- `protoc` (the Protocol Buffers compiler), needed by Lance's build scripts:
  `apt install protobuf-compiler`, `brew install protobuf`, or
  `choco install protoc`.
- On Linux, the usual runtime libraries for iced/winit: `libxkbcommon` plus
  `libxkbcommon-x11` on X11 (desktop installs have them; minimal containers may
  not), and a working `xdg-desktop-portal` for the Browse… and Export CSV
  dialogs.

```sh
cd joust
cargo run --release                      # start empty
cargo run --release -- ~/data/my.lancedb # open a database on start-up
```

Use `--release`: DataFusion and Lance are dramatically slower in debug builds.
The first build compiles several hundred crates and takes a while.

> **Note:** joust enables LanceDB's `remote` feature only because
> `lancedb` 0.39.0 does not compile without it. Only local (directory / object
> store URI) databases have been exercised.

## Using it

1. Click **Sample database** (or type a path and press Enter / **Open**, or
   **Browse…**).
2. Pick one of the example queries, or write your own and press `Ctrl+Enter`.
3. Switch between **Results**, **Columns**, **Media** (when the result has
   images), **Plan** and **Chart** tabs.
4. **Import media…** turns a folder of your own images, audio, video or PDFs
   into a table.

Some queries to try against the sample database:

```sql
-- Nearest neighbours in the synthetic embedding space
SELECT title, year, director, _distance
FROM vector_search('movies', 'vector', '[0.75, 0.1, 0, 0, 0, 0.55, 0, 0.35]', 8, 'cosine')
ORDER BY _distance;

-- Full-text search (the sample creates the FTS index)
SELECT id, title, year, _score
FROM fts('movies', '{"match": {"column": "title", "terms": "die hard"}}')
ORDER BY _score DESC;

-- Daily activity — open the Chart tab afterwards
SELECT date_trunc('day', ts) AS day, count(*) AS events, round(sum(amount_usd), 2) AS revenue_usd
FROM events GROUP BY 1 ORDER BY 1;

-- Media functions on the poster images (open the Media tab to browse them)
SELECT title, media_type(poster) AS mime, image_width(poster) AS w,
       image_height(poster) AS h, byte_length(poster) AS bytes
FROM movies ORDER BY bytes DESC LIMIT 10;

-- Persist a query result as a new Lance table
CREATE TABLE miyazaki AS
SELECT title, year FROM movies WHERE director = 'Hayao Miyazaki';
```

Identifiers follow SQL rules: unquoted names are lower-cased, so a table
called `MyTable` must be written `"MyTable"` (the sidebar inserts quoted names
for you).

## Keyboard shortcuts

| Keys | Action |
|---|---|
| `Ctrl+Enter` / `F5` | Run the editor (or the selection) |
| `Esc` | Cancel the running query (also works while typing in the editor) |
| Arrow keys, `PageUp`/`PageDown`, `Home`/`End` | Move the grid selection (after clicking the grid) |
| `Ctrl+Home` / `Ctrl+End` | First / last row |
| `Ctrl+C` (grid focused) | Copy the selected value |
| `Shift`+scroll | Scroll the grid horizontally |
| `Ctrl`+scroll (plan) | Zoom the plan diagram |

On macOS use `Cmd` instead of `Ctrl`.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests build the sample database in a temporary directory and exercise the
SQL layer end to end (catalog, plans, `vector_search`, `fts`, CTAS / `INSERT` /
`DROP`, row limits, media functions, folder import and colour search) along
with media sniffing, thumbnails, the highlighter, profiler, chart scales and
layout helpers.

### Layout

| Path | What |
|---|---|
| `src/db/mod.rs` | Connection, catalog sync, SQL execution, CTAS/DROP interception, indexing |
| `src/db/functions.rs` | `vector_search` and `fts` SQL table functions |
| `src/db/media_udf.rs` | `media_type`, `image_width`, `image_height`, `byte_length` |
| `src/db/import.rs` | Media folder import |
| `src/db/plan.rs` | Snapshot of the executed physical plan with metrics |
| `src/db/sample.rs` | Sample database generator |
| `src/app.rs` | Application state, messages and update loop |
| `src/ui/` | Views: layout, grid, media gallery, plan diagram, charts, column cards |
| `src/media.rs` | Media sniffing, image metadata, thumbnails, colour vectors |
| `src/theme.rs` | Theme catalog and widget styles |
| `src/highlight.rs` | SQL syntax highlighter |
| `src/profile.rs` | Column profiling |
| `src/results.rs` | Result formatting, sorting, CSV export |
