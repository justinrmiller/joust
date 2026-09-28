//! A small demo database so joust is useful before you have your own data.
//!
//! * `movies`: real titles, years and directors, with a *synthetic* 8-d
//!   `vector` derived from genre tags and release year, so that
//!   `vector_search` returns sensible neighbours. Each film also has a
//!   procedurally generated `poster` (abstract genre-palette art, not the real
//!   poster) and its `poster_colors` vector, for trying out the multimodal
//!   features.
//! * `events`: 50k rows of generated product-analytics events, handy for
//!   charts, aggregates and the column profiler.
//! * `trailers` (only when ffmpeg is available to encode them): one short,
//!   procedurally generated MP4 per genre — animated gradients in the genre's
//!   colours, not real trailers — with a colour vector of each poster frame.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use image::{DynamicImage, Rgb, RgbImage};
use lancedb::arrow::arrow_array::builder::{ListBuilder, StringBuilder};
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_array::{
    ArrayRef, BooleanArray, FixedSizeListArray, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use lancedb::arrow::arrow_array::{BinaryArray, LargeBinaryArray};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, TimeUnit};
use lancedb::database::CreateTableMode;
use lancedb::index::Index;
use lancedb::index::scalar::FtsIndexBuilder;

use crate::media::{COLOR_DIM, color_vector, encode_png};

/// Dimension of the synthetic movie embeddings.
pub const MOVIE_VECTOR_DIM: i32 = 8;
/// Number of generated rows in the `events` table.
pub const EVENT_ROWS: usize = 50_000;

/// `(title, year, director, genre tags)`; the first tag is the primary genre.
type Movie = (&'static str, i32, &'static str, &'static [&'static str]);

const MOVIES: &[Movie] = &[
    ("Citizen Kane", 1941, "Orson Welles", &["drama", "mystery"]),
    (
        "2001: A Space Odyssey",
        1968,
        "Stanley Kubrick",
        &["sci-fi", "drama"],
    ),
    (
        "Butch Cassidy and the Sundance Kid",
        1969,
        "George Roy Hill",
        &["western", "comedy", "action"],
    ),
    ("Alien", 1979, "Ridley Scott", &["sci-fi", "horror"]),
    (
        "Blade Runner",
        1982,
        "Ridley Scott",
        &["sci-fi", "thriller"],
    ),
    (
        "Nausicaä of the Valley of the Wind",
        1984,
        "Hayao Miyazaki",
        &["animation", "fantasy", "sci-fi"],
    ),
    (
        "The Terminator",
        1984,
        "James Cameron",
        &["sci-fi", "action"],
    ),
    ("This Is Spinal Tap", 1984, "Rob Reiner", &["comedy"]),
    ("Commando", 1985, "Mark L. Lester", &["action"]),
    (
        "Aliens",
        1986,
        "James Cameron",
        &["sci-fi", "action", "horror"],
    ),
    (
        "Castle in the Sky",
        1986,
        "Hayao Miyazaki",
        &["animation", "fantasy", "adventure"],
    ),
    ("Stand by Me", 1986, "Rob Reiner", &["drama", "adventure"]),
    ("Predator", 1987, "John McTiernan", &["action", "sci-fi"]),
    (
        "The Princess Bride",
        1987,
        "Rob Reiner",
        &["fantasy", "adventure", "comedy"],
    ),
    (
        "The Running Man",
        1987,
        "Paul Michael Glaser",
        &["action", "sci-fi"],
    ),
    ("Die Hard", 1988, "John McTiernan", &["action", "thriller"]),
    (
        "My Neighbor Totoro",
        1988,
        "Hayao Miyazaki",
        &["animation", "fantasy"],
    ),
    (
        "Bill & Ted's Excellent Adventure",
        1989,
        "Stephen Herek",
        &["comedy", "sci-fi", "adventure"],
    ),
    (
        "Kiki's Delivery Service",
        1989,
        "Hayao Miyazaki",
        &["animation", "fantasy"],
    ),
    (
        "When Harry Met Sally...",
        1989,
        "Rob Reiner",
        &["romance", "comedy"],
    ),
    ("Die Hard 2", 1990, "Renny Harlin", &["action", "thriller"]),
    (
        "Kindergarten Cop",
        1990,
        "Ivan Reitman",
        &["comedy", "action"],
    ),
    ("Misery", 1990, "Rob Reiner", &["thriller", "horror"]),
    (
        "Total Recall",
        1990,
        "Paul Verhoeven",
        &["sci-fi", "action"],
    ),
    (
        "Bill & Ted's Bogus Journey",
        1991,
        "Peter Hewitt",
        &["comedy", "sci-fi", "adventure"],
    ),
    (
        "Terminator 2: Judgment Day",
        1991,
        "James Cameron",
        &["sci-fi", "action"],
    ),
    ("A Few Good Men", 1992, "Rob Reiner", &["drama", "thriller"]),
    ("Alien 3", 1992, "David Fincher", &["sci-fi", "horror"]),
    (
        "Porco Rosso",
        1992,
        "Hayao Miyazaki",
        &["animation", "adventure"],
    ),
    ("True Lies", 1994, "James Cameron", &["action", "comedy"]),
    (
        "Die Hard with a Vengeance",
        1995,
        "John McTiernan",
        &["action", "thriller"],
    ),
    ("Bottle Rocket", 1996, "Wes Anderson", &["comedy", "drama"]),
    (
        "Alien Resurrection",
        1997,
        "Jean-Pierre Jeunet",
        &["sci-fi", "horror"],
    ),
    ("Gattaca", 1997, "Andrew Niccol", &["sci-fi", "drama"]),
    (
        "Princess Mononoke",
        1997,
        "Hayao Miyazaki",
        &["animation", "fantasy", "adventure"],
    ),
    ("Rushmore", 1998, "Wes Anderson", &["comedy", "drama"]),
    ("The Matrix", 1999, "The Wachowskis", &["sci-fi", "action"]),
    (
        "The 6th Day",
        2000,
        "Roger Spottiswoode",
        &["sci-fi", "action", "thriller"],
    ),
    ("Antitrust", 2001, "Peter Howitt", &["thriller", "drama"]),
    (
        "Spirited Away",
        2001,
        "Hayao Miyazaki",
        &["animation", "fantasy"],
    ),
    (
        "The Royal Tenenbaums",
        2001,
        "Wes Anderson",
        &["comedy", "drama"],
    ),
    (
        "Howl's Moving Castle",
        2004,
        "Hayao Miyazaki",
        &["animation", "fantasy", "romance"],
    ),
    (
        "The Life Aquatic with Steve Zissou",
        2004,
        "Wes Anderson",
        &["comedy", "adventure"],
    ),
    (
        "Live Free or Die Hard",
        2007,
        "Len Wiseman",
        &["action", "thriller"],
    ),
    (
        "The Darjeeling Limited",
        2007,
        "Wes Anderson",
        &["comedy", "drama"],
    ),
    ("Ponyo", 2008, "Hayao Miyazaki", &["animation", "fantasy"]),
    (
        "Fantastic Mr. Fox",
        2009,
        "Wes Anderson",
        &["animation", "comedy"],
    ),
    ("Moon", 2009, "Duncan Jones", &["sci-fi", "drama"]),
    (
        "Moonrise Kingdom",
        2012,
        "Wes Anderson",
        &["comedy", "romance"],
    ),
    ("Prometheus", 2012, "Ridley Scott", &["sci-fi", "horror"]),
    (
        "A Good Day to Die Hard",
        2013,
        "John Moore",
        &["action", "thriller"],
    ),
    (
        "The Wind Rises",
        2013,
        "Hayao Miyazaki",
        &["animation", "drama"],
    ),
    (
        "The Grand Budapest Hotel",
        2014,
        "Wes Anderson",
        &["comedy", "adventure"],
    ),
    ("Arrival", 2016, "Denis Villeneuve", &["sci-fi", "drama"]),
    (
        "Alien: Covenant",
        2017,
        "Ridley Scott",
        &["sci-fi", "horror"],
    ),
    (
        "Blade Runner 2049",
        2017,
        "Denis Villeneuve",
        &["sci-fi", "thriller"],
    ),
    (
        "Isle of Dogs",
        2018,
        "Wes Anderson",
        &["animation", "comedy"],
    ),
    (
        "Bill & Ted Face the Music",
        2020,
        "Dean Parisot",
        &["comedy", "sci-fi"],
    ),
    ("Dune", 2021, "Denis Villeneuve", &["sci-fi", "adventure"]),
    (
        "The French Dispatch",
        2021,
        "Wes Anderson",
        &["comedy", "drama"],
    ),
    ("Asteroid City", 2023, "Wes Anderson", &["comedy", "sci-fi"]),
    (
        "The Boy and the Heron",
        2023,
        "Hayao Miyazaki",
        &["animation", "fantasy"],
    ),
    (
        "Alien: Romulus",
        2024,
        "Fede Álvarez",
        &["sci-fi", "horror"],
    ),
    (
        "Dune: Part Two",
        2024,
        "Denis Villeneuve",
        &["sci-fi", "adventure", "action"],
    ),
];

/// Genre axes of the synthetic embedding (the 8th axis is release era).
const GENRE_AXES: [&[&str]; 7] = [
    &["sci-fi"],
    &["action", "western"],
    &["animation"],
    &["comedy"],
    &["drama", "romance", "mystery"],
    &["horror", "thriller"],
    &["fantasy", "adventure"],
];

/// Creates (or overwrites) the demo tables in the database at `path`.
/// Returns how many trailer videos were generated (0 without ffmpeg).
pub async fn create_sample_database(path: &Path) -> Result<usize> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("could not create {}", path.display()))?;
    let uri = path.to_string_lossy();
    let connection = lancedb::connect(&uri).execute().await?;

    let movies = connection
        .create_table("movies", movies_batch()?)
        .mode(CreateTableMode::Overwrite)
        .execute()
        .await
        .context("could not create the movies table")?;
    movies
        .create_index(&["title"], Index::FTS(FtsIndexBuilder::default()))
        .execute()
        .await
        .context("could not build the movies full-text index")?;

    connection
        .create_table("events", events_batch()?)
        .mode(CreateTableMode::Overwrite)
        .execute()
        .await
        .context("could not create the events table")?;

    let trailers = tokio::task::spawn_blocking(trailers_batch).await??;
    let count = trailers.as_ref().map_or(0, RecordBatch::num_rows);
    match trailers {
        Some(batch) => {
            connection
                .create_table("trailers", batch)
                .mode(CreateTableMode::Overwrite)
                .execute()
                .await
                .context("could not create the trailers table")?;
        }
        None => {
            // Don't leave a stale table from a run that had ffmpeg.
            if connection
                .table_names()
                .execute()
                .await?
                .iter()
                .any(|t| t == "trailers")
            {
                connection.drop_table("trailers", &[]).await?;
            }
        }
    }
    Ok(count)
}

/// Seconds per generated trailer.
pub const TRAILER_SECONDS: f64 = 4.0;
/// Trailer frame size.
pub const TRAILER_SIZE: (u32, u32) = (320, 180);

/// One generated clip per primary genre (first film of each genre), or
/// `None` when ffmpeg is unavailable.
pub fn trailers_batch() -> Result<Option<RecordBatch>> {
    if !crate::ffmpeg::available() {
        return Ok(None);
    }
    let mut picks: Vec<(usize, &Movie)> = Vec::new();
    for (index, movie) in MOVIES.iter().enumerate() {
        if !picks.iter().any(|(_, m)| m.3[0] == movie.3[0]) {
            picks.push((index, movie));
        }
    }

    let to_hex = |[r, g, b]: [u8; 3]| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    let clips: Vec<Option<(Vec<u8>, [f32; COLOR_DIM])>> = std::thread::scope(|scope| {
        let handles: Vec<_> = picks
            .iter()
            .map(|(_, (title, _, _, tags))| {
                scope.spawn(move || {
                    let (top, bottom, accent) = genre_palette(tags[0]);
                    let seed = title
                        .bytes()
                        .fold(5u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
                    let clip = crate::ffmpeg::synthesize_clip(
                        TRAILER_SECONDS,
                        TRAILER_SIZE,
                        [to_hex(top), to_hex(accent), to_hex(bottom)],
                        seed % 10_000,
                    )?;
                    let still = crate::media::MediaValue::Bytes(clip.clone()).still(64)?;
                    Some((clip, color_vector(&still)))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().ok().flatten())
            .collect()
    });

    let rows: Vec<_> = picks
        .iter()
        .zip(clips)
        .filter_map(|((index, movie), clip)| clip.map(|clip| (*index, *movie, clip)))
        .collect();
    if rows.is_empty() {
        return Ok(None);
    }

    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("movie_id", DataType::Int32, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("genre", DataType::Utf8, false),
        Field::new("clip", DataType::LargeBinary, false),
        Field::new(
            "clip_colors",
            DataType::FixedSizeList(item, COLOR_DIM as i32),
            true,
        ),
    ]);
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int32Array::from_iter_values(1..=rows.len() as i32)),
        Arc::new(Int32Array::from_iter_values(
            rows.iter().map(|(index, _, _)| *index as i32 + 1),
        )),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|(_, m, _)| m.0),
        )),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|(_, m, _)| m.3[0]),
        )),
        Arc::new(LargeBinaryArray::from_iter_values(
            rows.iter().map(|(_, _, (clip, _))| clip.as_slice()),
        )),
        Arc::new(
            FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                rows.iter()
                    .map(|(_, _, (_, colors))| Some(colors.map(Some))),
                COLOR_DIM as i32,
            ),
        ),
    ];
    Ok(Some(RecordBatch::try_new(Arc::new(schema), columns)?))
}

/// Builds the `movies` record batch.
pub fn movies_batch() -> Result<RecordBatch> {
    let mut tags = ListBuilder::new(StringBuilder::new());
    for (_, _, _, movie_tags) in MOVIES {
        for tag in *movie_tags {
            tags.values().append_value(tag);
        }
        tags.append(true);
    }

    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        MOVIES
            .iter()
            .map(|(title, year, _, tags)| Some(movie_vector(title, *year, tags).map(Some))),
        MOVIE_VECTOR_DIM,
    );

    let posters: Vec<DynamicImage> = MOVIES
        .iter()
        .map(|(title, year, _, tags)| poster(title, *year, tags))
        .collect();
    let poster_bytes = posters
        .iter()
        .map(|image| encode_png(image).context("could not encode a poster"))
        .collect::<Result<Vec<_>>>()?;
    let poster_colors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        posters
            .iter()
            .map(|image| Some(color_vector(image).map(Some))),
        COLOR_DIM as i32,
    );

    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int32Array::from_iter_values(1..=MOVIES.len() as i32)),
        Arc::new(StringArray::from_iter_values(MOVIES.iter().map(|m| m.0))),
        Arc::new(Int32Array::from_iter_values(MOVIES.iter().map(|m| m.1))),
        Arc::new(StringArray::from_iter_values(MOVIES.iter().map(|m| m.2))),
        Arc::new(StringArray::from_iter_values(MOVIES.iter().map(|m| m.3[0]))),
        Arc::new(tags.finish()),
        Arc::new(vectors),
        Arc::new(BinaryArray::from_iter_values(poster_bytes.iter())),
        Arc::new(poster_colors),
    ];
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let tag_item = Arc::new(Field::new("item", DataType::Utf8, true));
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("year", DataType::Int32, false),
        Field::new("director", DataType::Utf8, false),
        Field::new("genre", DataType::Utf8, false),
        Field::new("tags", DataType::List(tag_item), true),
        Field::new(
            "vector",
            DataType::FixedSizeList(item.clone(), MOVIE_VECTOR_DIM),
            true,
        ),
        Field::new("poster", DataType::Binary, true),
        Field::new(
            "poster_colors",
            DataType::FixedSizeList(item, COLOR_DIM as i32),
            true,
        ),
    ]);
    Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
}

/// Poster size in pixels.
pub const POSTER_SIZE: (u32, u32) = (120, 180);

/// `(top, bottom, accent)` colours for a genre.
fn genre_palette(genre: &str) -> ([u8; 3], [u8; 3], [u8; 3]) {
    match genre {
        "sci-fi" => ([10, 18, 48], [18, 90, 110], [90, 220, 240]),
        "horror" => ([8, 6, 8], [90, 10, 14], [220, 30, 40]),
        "action" => ([60, 20, 8], [200, 70, 20], [255, 200, 40]),
        "animation" => ([120, 190, 240], [190, 235, 190], [255, 255, 255]),
        "comedy" => ([255, 210, 60], [250, 130, 40], [230, 40, 140]),
        "drama" => ([40, 48, 64], [110, 30, 50], [230, 190, 90]),
        "thriller" => ([18, 22, 24], [20, 70, 50], [170, 230, 60]),
        "fantasy" => ([70, 30, 120], [220, 110, 170], [255, 215, 90]),
        "western" => ([230, 190, 130], [160, 70, 30], [90, 40, 20]),
        "romance" => ([240, 150, 170], [170, 140, 220], [220, 30, 70]),
        "mystery" => ([20, 60, 70], [10, 12, 16], [230, 230, 230]),
        "adventure" => ([40, 120, 70], [120, 190, 230], [250, 150, 40]),
        _ => ([60, 60, 60], [120, 120, 120], [240, 240, 240]),
    }
}

fn mix_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [0, 1, 2].map(|i| (f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t).round() as u8)
}

/// Procedural poster: a gradient in the primary genre's colours, a "sun"
/// and bands in the secondary genre's accent, and a title block whose bars
/// follow the title's word lengths. Older films are faded towards sepia.
fn poster(title: &str, year: i32, tags: &[&str]) -> DynamicImage {
    let (w, h) = POSTER_SIZE;
    let (top, bottom, accent) = genre_palette(tags[0]);
    let second_accent = tags.get(1).map_or(accent, |tag| genre_palette(tag).2);
    let mut rng = SplitMix64::new(
        title
            .bytes()
            .fold(17u64, |h, b| h.wrapping_mul(131) ^ u64::from(b)),
    );

    let cx = 25.0 + rng.next_f64() as f32 * 70.0;
    let cy = 30.0 + rng.next_f64() as f32 * 60.0;
    let radius = 16.0 + rng.next_f64() as f32 * 22.0;
    let band_period = 9.0 + rng.next_f64() as f32 * 14.0;
    let fade = ((1980 - year) as f32 / 45.0).clamp(0.0, 0.6);

    let mut image = RgbImage::new(w, h);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (fx, fy) = (x as f32, y as f32);
        let mut color = mix_rgb(top, bottom, fy / (h - 1) as f32);
        // Diagonal bands in the secondary accent.
        if ((fx + fy * 0.6) / band_period).floor() as i32 % 5 == 0 && fy < 135.0 {
            color = mix_rgb(color, second_accent, 0.22);
        }
        // Soft-edged sun / planet.
        let distance = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
        let coverage = (radius + 1.0 - distance).clamp(0.0, 1.0);
        color = mix_rgb(color, accent, coverage * 0.9);
        // Title block.
        if fy >= 138.0 {
            color = mix_rgb(color, [12, 12, 14], 0.8);
        }
        // Age the colours of older films.
        let grey = (u32::from(color[0]) * 3 + u32::from(color[1]) * 6 + u32::from(color[2])) / 10;
        let sepia = [grey + 30, grey + 12, grey].map(|c| c.min(255) as u8);
        *pixel = Rgb(mix_rgb(color, sepia, fade));
    }

    // One light bar per title word (up to three), sized by word length.
    for (line, word) in title.split_whitespace().take(3).enumerate() {
        let length = (word.chars().count() as u32 * 9).clamp(10, w - 20);
        let y0 = 146 + line as u32 * 10;
        for y in y0..(y0 + 5).min(h) {
            for x in 10..10 + length {
                image.put_pixel(x, y, Rgb([235, 232, 225]));
            }
        }
    }
    DynamicImage::ImageRgb8(image)
}

/// Synthetic, L2-normalised embedding: genre weights plus release era.
fn movie_vector(title: &str, year: i32, tags: &[&str]) -> [f32; 8] {
    let mut vector = [0f32; 8];
    for (rank, tag) in tags.iter().enumerate() {
        let weight = 1.0 / (rank as f32 + 1.0);
        for (axis, names) in GENRE_AXES.iter().enumerate() {
            if names.contains(tag) {
                vector[axis] += weight;
            }
        }
    }
    vector[7] = (year - 1940) as f32 / 85.0;

    // A little deterministic per-title jitter so ties are broken stably.
    let mut rng = SplitMix64::new(
        title
            .bytes()
            .fold(7u64, |h, b| h.wrapping_mul(31) ^ u64::from(b)),
    );
    for value in &mut vector {
        *value += (rng.next_f64() as f32 - 0.5) * 0.05;
    }

    let norm = vector
        .iter()
        .map(|v| v * v)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    vector.map(|v| v / norm)
}

/// Builds the generated `events` record batch.
pub fn events_batch() -> Result<RecordBatch> {
    const EVENTS: [(&str, f64); 5] = [
        ("page_view", 0.60),
        ("search", 0.20),
        ("add_to_cart", 0.10),
        ("purchase", 0.05),
        ("signup", 0.05),
    ];
    const COUNTRIES: [(&str, f64); 10] = [
        ("US", 0.30),
        ("DE", 0.10),
        ("JP", 0.10),
        ("GB", 0.09),
        ("IN", 0.09),
        ("BR", 0.08),
        ("FR", 0.07),
        ("CA", 0.07),
        ("KR", 0.05),
        ("AU", 0.05),
    ];
    // 2026-01-01T00:00:00Z in microseconds.
    const START_MICROS: i64 = 1_767_225_600_000_000;
    const SPAN_MICROS: f64 = 60.0 * 24.0 * 3600.0 * 1e6;

    let mut rng = SplitMix64::new(0x006a_6f75_7374);
    let mut timestamps = Vec::with_capacity(EVENT_ROWS);
    for _ in 0..EVENT_ROWS {
        // Skew traffic towards later days to give the chart a trend.
        let t = rng.next_f64().sqrt();
        timestamps.push(START_MICROS + (t * SPAN_MICROS) as i64);
    }
    timestamps.sort_unstable();

    let mut user_ids = Vec::with_capacity(EVENT_ROWS);
    let mut kinds = Vec::with_capacity(EVENT_ROWS);
    let mut countries = Vec::with_capacity(EVENT_ROWS);
    let mut latencies = Vec::with_capacity(EVENT_ROWS);
    let mut amounts = Vec::with_capacity(EVENT_ROWS);
    let mut mobile = Vec::with_capacity(EVENT_ROWS);
    for _ in 0..EVENT_ROWS {
        user_ids.push(1 + (rng.next_f64().powi(2) * 4_999.0) as i32);
        let kind = pick(&mut rng, &EVENTS);
        kinds.push(kind);
        countries.push(pick(&mut rng, &COUNTRIES));
        let is_mobile = rng.next_f64() < 0.58;
        mobile.push(is_mobile);
        // Log-normal latency, slower on mobile and for searches.
        let base = if kind == "search" { 4.6 } else { 4.1 };
        let base = if is_mobile { base + 0.25 } else { base };
        latencies.push((base + 0.45 * rng.next_gaussian()).exp());
        amounts.push((kind == "purchase").then(|| {
            let dollars = (3.2 + 0.8 * rng.next_gaussian()).exp();
            (dollars * 100.0).round() / 100.0
        }));
    }

    let schema = Schema::new(vec![
        Field::new("event_id", DataType::Int64, false),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("user_id", DataType::Int32, false),
        Field::new("event", DataType::Utf8, false),
        Field::new("country", DataType::Utf8, false),
        Field::new("is_mobile", DataType::Boolean, false),
        Field::new("latency_ms", DataType::Float64, false),
        Field::new("amount_usd", DataType::Float64, true),
    ]);
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(1..=EVENT_ROWS as i64)),
        Arc::new(TimestampMicrosecondArray::from(timestamps).with_timezone("UTC")),
        Arc::new(Int32Array::from(user_ids)),
        Arc::new(StringArray::from(kinds)),
        Arc::new(StringArray::from(countries)),
        Arc::new(BooleanArray::from(mobile)),
        Arc::new(Float64Array::from(latencies)),
        Arc::new(Float64Array::from(amounts)),
    ];
    Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
}

/// Picks a value from `(value, weight)` pairs whose weights sum to ~1.
fn pick<'a>(rng: &mut SplitMix64, weighted: &[(&'a str, f64)]) -> &'a str {
    let mut roll = rng.next_f64();
    for (value, weight) in weighted {
        if roll < *weight {
            return value;
        }
        roll -= weight;
    }
    weighted[weighted.len() - 1].0
}

/// Tiny deterministic PRNG (SplitMix64) so the sample data is reproducible.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal via Box–Muller.
    fn next_gaussian(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movies_batch_is_well_formed() {
        let batch = movies_batch().unwrap();
        assert_eq!(batch.num_rows(), MOVIES.len());
        assert_eq!(batch.num_columns(), 9);
        let posters = batch
            .column(7)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        assert_eq!(
            crate::media::image_dimensions(posters.value(0)),
            Some(POSTER_SIZE)
        );
    }

    #[test]
    fn movie_vectors_are_unit_length_and_cluster_by_genre() {
        let alien = movie_vector("Alien", 1979, &["sci-fi", "horror"]);
        let aliens = movie_vector("Aliens", 1986, &["sci-fi", "action", "horror"]);
        let totoro = movie_vector("My Neighbor Totoro", 1988, &["animation", "fantasy"]);
        let norm: f32 = alien.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        let dot = |a: &[f32; 8], b: &[f32; 8]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&alien, &aliens) > dot(&alien, &totoro));
    }

    #[test]
    fn trailers_cover_each_genre_when_ffmpeg_exists() {
        let Some(batch) = trailers_batch().unwrap() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let genres: std::collections::HashSet<&str> = MOVIES.iter().map(|m| m.3[0]).collect();
        assert_eq!(batch.num_rows(), genres.len());
        let clips = batch
            .column(4)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();
        let info = crate::av::info(clips.value(0)).unwrap();
        assert_eq!((info.width, info.height), (Some(320), Some(180)));
        assert!((info.duration.unwrap() - TRAILER_SECONDS).abs() < 0.2);
    }

    #[test]
    fn events_batch_is_deterministic() {
        let a = events_batch().unwrap();
        let b = events_batch().unwrap();
        assert_eq!(a.num_rows(), EVENT_ROWS);
        assert_eq!(a, b);
    }
}
