//! Column profiling for the "Columns" explorer.
//!
//! Profiles are computed off the UI thread after each query and summarise
//! every result column: null rate, distinct count, a histogram for numbers,
//! timestamps and vector norms, or the most frequent values for text.

use std::collections::HashMap;

use lancedb::arrow::arrow_array::cast::AsArray;
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch};
use lancedb::arrow::arrow_cast::cast;
use lancedb::arrow::arrow_schema::DataType;

use crate::results::{format_value, type_label};

/// Number of histogram bins.
pub const BINS: usize = 24;
/// Distinct values are counted exactly up to this many.
pub const DISTINCT_CAP: usize = 100_000;
/// How many frequent values text columns report.
pub const TOP_VALUES: usize = 6;

/// Summary of one result column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnProfile {
    pub name: String,
    pub type_label: String,
    pub rows: usize,
    pub nulls: usize,
    pub distinct: Distinct,
    pub summary: Summary,
}

impl ColumnProfile {
    /// Share of null values, 0–1.
    pub fn null_fraction(&self) -> f32 {
        if self.rows == 0 {
            0.0
        } else {
            self.nulls as f32 / self.rows as f32
        }
    }
}

/// Number of distinct non-null values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distinct {
    Exact(usize),
    /// Counting stopped at [`DISTINCT_CAP`].
    MoreThan(usize),
    /// Not counted for this type (vectors, media).
    NotComputed,
}

impl Distinct {
    /// `None` from the counting helpers means the cap was exceeded.
    fn counted(count: Option<usize>) -> Self {
        count.map_or(Distinct::MoreThan(DISTINCT_CAP), Distinct::Exact)
    }
}

/// Type-specific part of a [`ColumnProfile`].
#[derive(Debug, Clone, PartialEq)]
pub enum Summary {
    Numeric {
        min: f64,
        max: f64,
        mean: f64,
        std_dev: f64,
        histogram: Histogram,
    },
    Temporal {
        min: String,
        max: String,
        histogram: Histogram,
    },
    Text {
        min_len: usize,
        max_len: usize,
        mean_len: f64,
        top: Vec<(String, usize)>,
    },
    Boolean {
        true_count: usize,
        false_count: usize,
    },
    Vector {
        dimension: usize,
        min_norm: f64,
        max_norm: f64,
        mean_norm: f64,
        histogram: Histogram,
    },
    /// Binary columns: what kind of media they hold.
    Media {
        /// `(format, count)`, most common first, e.g. `("PNG image", 64)`.
        formats: Vec<(String, usize)>,
        min_bytes: usize,
        max_bytes: usize,
        mean_bytes: f64,
        /// `((min_w, min_h), (max_w, max_h))` over the images and videos.
        dimensions: Option<((u32, u32), (u32, u32))>,
        /// `(shortest, longest)` duration in seconds (video and audio).
        durations: Option<(f64, f64)>,
        /// Byte-size distribution.
        histogram: Histogram,
    },
    /// Nothing type-specific to show (e.g. structs, all-null columns).
    Other,
}

/// Equal-width histogram over `[min, max]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    pub min: f64,
    pub max: f64,
    pub counts: Vec<usize>,
}

impl Histogram {
    /// Bins `values` into [`BINS`] buckets. Returns `None` for no values.
    pub fn from_values(values: impl Iterator<Item = f64> + Clone) -> Option<Self> {
        let (min, max) = values
            .clone()
            .filter(|v| v.is_finite())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(v), hi.max(v))
            });
        if !min.is_finite() {
            return None;
        }
        let mut counts = vec![0usize; BINS];
        let width = (max - min) / BINS as f64;
        for value in values.filter(|v| v.is_finite()) {
            let bin = if width > 0.0 {
                (((value - min) / width) as usize).min(BINS - 1)
            } else {
                BINS / 2
            };
            counts[bin] += 1;
        }
        Some(Self { min, max, counts })
    }

    pub fn max_count(&self) -> usize {
        self.counts.iter().copied().max().unwrap_or(0)
    }
}

/// Profiles every column of `batch`.
pub fn profile_batch(batch: &RecordBatch) -> Vec<ColumnProfile> {
    batch
        .schema()
        .fields()
        .iter()
        .zip(batch.columns())
        .map(|(field, column)| profile_column(field.name(), column))
        .collect()
}

/// Profiles a single column.
pub fn profile_column(name: &str, column: &ArrayRef) -> ColumnProfile {
    let rows = column.len();
    let nulls = column.logical_null_count();
    let data_type = column.data_type();

    let counted = |(summary, count): (Summary, Option<usize>)| (summary, Distinct::counted(count));
    let (summary, distinct) = if nulls == rows {
        (Summary::Other, Distinct::Exact(0))
    } else if matches!(data_type, DataType::Boolean) {
        counted(boolean_summary(column))
    } else if data_type.is_numeric() {
        counted(numeric_summary(column))
    } else if data_type.is_temporal() {
        counted(temporal_summary(column))
    } else if matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View | DataType::Dictionary(..)
    ) {
        counted(text_summary(column))
    } else if let DataType::FixedSizeList(_, dimension) = data_type {
        (
            vector_summary(column, *dimension as usize),
            Distinct::NotComputed,
        )
    } else if crate::results::is_binary(data_type) {
        (media_summary(column), Distinct::NotComputed)
    } else {
        (
            Summary::Other,
            Distinct::counted(distinct_by_display(column)),
        )
    };

    ColumnProfile {
        name: name.to_string(),
        type_label: type_label(data_type),
        rows,
        nulls,
        distinct,
        summary,
    }
}

fn numeric_summary(column: &ArrayRef) -> (Summary, Option<usize>) {
    let Ok(values) = cast(column, &DataType::Float64) else {
        return (Summary::Other, None);
    };
    let values: &Float64Array = values.as_primitive();
    let finite = || values.iter().flatten().filter(|v| v.is_finite());

    let count = finite().count();
    if count == 0 {
        return (Summary::Other, None);
    }
    let mean = finite().sum::<f64>() / count as f64;
    let variance = finite().map(|v| (v - mean).powi(2)).sum::<f64>() / count as f64;
    let histogram = Histogram::from_values(finite()).expect("non-empty");

    let mut distinct = std::collections::HashSet::new();
    let mut capped = false;
    for value in values.iter().flatten() {
        distinct.insert(value.to_bits());
        if distinct.len() > DISTINCT_CAP {
            capped = true;
            break;
        }
    }

    (
        Summary::Numeric {
            min: histogram.min,
            max: histogram.max,
            mean,
            std_dev: variance.sqrt(),
            histogram,
        },
        (!capped).then_some(distinct.len()),
    )
}

fn temporal_summary(column: &ArrayRef) -> (Summary, Option<usize>) {
    let Ok(values) = cast(column, &DataType::Int64) else {
        return (Summary::Other, distinct_by_display(column));
    };
    let values: &Int64Array = values.as_primitive();

    let mut min_row = None;
    let mut max_row = None;
    for (row, value) in values.iter().enumerate() {
        let Some(value) = value else { continue };
        if min_row.is_none_or(|(_, min)| value < min) {
            min_row = Some((row, value));
        }
        if max_row.is_none_or(|(_, max)| value > max) {
            max_row = Some((row, value));
        }
    }
    let (Some((min_row, _)), Some((max_row, _))) = (min_row, max_row) else {
        return (Summary::Other, Some(0));
    };

    let histogram = Histogram::from_values(values.iter().flatten().map(|v| v as f64));
    let distinct: std::collections::HashSet<i64> =
        values.iter().flatten().take(DISTINCT_CAP + 1).collect();
    (
        Summary::Temporal {
            min: format_value(column.as_ref(), min_row),
            max: format_value(column.as_ref(), max_row),
            histogram: histogram.expect("non-empty"),
        },
        (distinct.len() <= DISTINCT_CAP).then_some(distinct.len()),
    )
}

fn text_summary(column: &ArrayRef) -> (Summary, Option<usize>) {
    let Ok(values) = cast(column, &DataType::Utf8) else {
        return (Summary::Other, None);
    };
    let values = values.as_string::<i32>();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut capped = false;
    let (mut min_len, mut max_len, mut total_len, mut n) = (usize::MAX, 0usize, 0usize, 0usize);
    for value in values.iter().flatten() {
        let len = value.chars().count();
        min_len = min_len.min(len);
        max_len = max_len.max(len);
        total_len += len;
        n += 1;
        if let Some(count) = counts.get_mut(value) {
            *count += 1;
        } else if counts.len() < DISTINCT_CAP {
            counts.insert(value, 1);
        } else {
            capped = true;
        }
    }

    let mut top: Vec<(String, usize)> = counts
        .iter()
        .map(|(value, count)| ((*value).to_string(), *count))
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(TOP_VALUES);

    (
        Summary::Text {
            min_len: if n == 0 { 0 } else { min_len },
            max_len,
            mean_len: if n == 0 {
                0.0
            } else {
                total_len as f64 / n as f64
            },
            top,
        },
        (!capped).then_some(counts.len()),
    )
}

fn boolean_summary(column: &ArrayRef) -> (Summary, Option<usize>) {
    let values = column.as_boolean();
    let true_count = values.true_count();
    let false_count = values.false_count();
    let distinct = usize::from(true_count > 0) + usize::from(false_count > 0);
    (
        Summary::Boolean {
            true_count,
            false_count,
        },
        Some(distinct),
    )
}

fn vector_summary(column: &ArrayRef, dimension: usize) -> Summary {
    let list = column.as_fixed_size_list();
    let Ok(values) = cast(list.values(), &DataType::Float32) else {
        return Summary::Other;
    };
    let values = values.as_primitive::<Float32Type>();

    let norms: Vec<f64> = (0..list.len())
        .filter(|row| list.is_valid(*row))
        .map(|row| {
            let start = row * dimension;
            (start..start + dimension)
                .map(|i| f64::from(values.value(i)).powi(2))
                .sum::<f64>()
                .sqrt()
        })
        .collect();
    let Some(histogram) = Histogram::from_values(norms.iter().copied()) else {
        return Summary::Other;
    };
    Summary::Vector {
        dimension,
        min_norm: histogram.min,
        max_norm: histogram.max,
        mean_norm: norms.iter().sum::<f64>() / norms.len() as f64,
        histogram,
    }
}

/// Rows whose headers are parsed for image dimensions (bounds the cost).
const DIMENSION_SAMPLE: usize = 2_000;

fn media_summary(column: &ArrayRef) -> Summary {
    use crate::media;
    use crate::results::binary_value;

    let mut formats: HashMap<String, usize> = HashMap::new();
    let mut sizes = Vec::new();
    let (mut min_dims, mut max_dims) = ((u32::MAX, u32::MAX), (0u32, 0u32));
    let mut measured = 0;
    let (mut shortest, mut longest) = (f64::INFINITY, f64::NEG_INFINITY);
    for row in 0..column.len() {
        let Some(bytes) = binary_value(column.as_ref(), row) else {
            continue;
        };
        sizes.push(bytes.len());
        let label = match media::sniff(bytes) {
            Some(format) => format!("{} {}", format.name, format.kind.label()),
            None => "other bytes".to_string(),
        };
        *formats.entry(label).or_default() += 1;
        if measured >= DIMENSION_SAMPLE {
            continue;
        }
        let av = crate::av::info(bytes);
        let dims = media::image_dimensions(bytes).or_else(|| {
            av.as_ref()
                .and_then(|info| Some((info.width?, info.height?)))
        });
        if let Some((w, h)) = dims {
            measured += 1;
            min_dims = (min_dims.0.min(w), min_dims.1.min(h));
            max_dims = (max_dims.0.max(w), max_dims.1.max(h));
        }
        if let Some(duration) = av.and_then(|info| info.duration) {
            shortest = shortest.min(duration);
            longest = longest.max(duration);
        }
    }
    let Some(histogram) = Histogram::from_values(sizes.iter().map(|&n| n as f64)) else {
        return Summary::Other;
    };
    let mut formats: Vec<(String, usize)> = formats.into_iter().collect();
    formats.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Summary::Media {
        formats,
        min_bytes: sizes.iter().copied().min().unwrap_or(0),
        max_bytes: sizes.iter().copied().max().unwrap_or(0),
        mean_bytes: sizes.iter().sum::<usize>() as f64 / sizes.len() as f64,
        dimensions: (measured > 0).then_some((min_dims, max_dims)),
        durations: shortest.is_finite().then_some((shortest, longest)),
        histogram,
    }
}

/// Distinct count of the formatted values (fallback for exotic types).
fn distinct_by_display(column: &ArrayRef) -> Option<usize> {
    let mut seen = std::collections::HashSet::new();
    for row in 0..column.len().min(DISTINCT_CAP + 1) {
        seen.insert(format_value(column.as_ref(), row));
    }
    (column.len() <= DISTINCT_CAP).then_some(seen.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use lancedb::arrow::arrow_array::{BooleanArray, FixedSizeListArray, Int32Array, StringArray};

    #[test]
    fn numeric_profile() {
        let column: ArrayRef = Arc::new(Int32Array::from(vec![
            Some(1),
            Some(2),
            None,
            Some(3),
            Some(3),
        ]));
        let profile = profile_column("n", &column);
        assert_eq!(profile.nulls, 1);
        assert_eq!(profile.distinct, Distinct::Exact(3));
        let Summary::Numeric {
            min,
            max,
            mean,
            histogram,
            ..
        } = profile.summary
        else {
            panic!("expected numeric summary");
        };
        assert_eq!((min, max, mean), (1.0, 3.0, 2.25));
        assert_eq!(histogram.counts.iter().sum::<usize>(), 4);
        assert_eq!(histogram.counts[BINS - 1], 2);
    }

    #[test]
    fn text_profile_ranks_top_values() {
        let column: ArrayRef = Arc::new(StringArray::from(vec!["b", "a", "b", "ccc"]));
        let profile = profile_column("s", &column);
        let Summary::Text {
            top,
            min_len,
            max_len,
            ..
        } = profile.summary
        else {
            panic!("expected text summary");
        };
        assert_eq!(top[0], ("b".to_string(), 2));
        assert_eq!((min_len, max_len), (1, 3));
        assert_eq!(profile.distinct, Distinct::Exact(3));
    }

    #[test]
    fn boolean_and_all_null() {
        let column: ArrayRef = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            Some(true),
        ]));
        let profile = profile_column("b", &column);
        assert_eq!(
            profile.summary,
            Summary::Boolean {
                true_count: 2,
                false_count: 1
            }
        );

        let column: ArrayRef = Arc::new(Int32Array::from(vec![None, None]));
        assert_eq!(profile_column("x", &column).summary, Summary::Other);
    }

    #[test]
    fn media_profile_counts_formats() {
        use lancedb::arrow::arrow_array::BinaryArray;
        let png = crate::media::encode_png(&image::DynamicImage::new_rgb8(4, 3)).unwrap();
        let column: ArrayRef = Arc::new(BinaryArray::from(vec![
            Some(png.as_slice()),
            Some(png.as_slice()),
            Some(&b"%PDF-1.4"[..]),
            None,
        ]));
        let profile = profile_column("m", &column);
        assert_eq!(profile.nulls, 1);
        let Summary::Media {
            formats,
            dimensions,
            min_bytes,
            durations,
            ..
        } = profile.summary
        else {
            panic!("expected media summary");
        };
        assert_eq!(formats[0], ("PNG image".to_string(), 2));
        assert_eq!(formats[1], ("PDF document".to_string(), 1));
        assert_eq!(dimensions, Some(((4, 3), (4, 3))));
        assert_eq!(min_bytes, 8);
        assert_eq!(durations, None);
    }

    #[test]
    fn vector_norms() {
        let column: ArrayRef = Arc::new(
            FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                vec![
                    Some(vec![Some(3.0), Some(4.0)]),
                    Some(vec![Some(0.0), Some(1.0)]),
                ],
                2,
            ),
        );
        let Summary::Vector {
            dimension,
            min_norm,
            max_norm,
            ..
        } = profile_column("v", &column).summary
        else {
            panic!("expected vector summary");
        };
        assert_eq!((dimension, min_norm, max_norm), (2, 1.0, 5.0));
    }
}
