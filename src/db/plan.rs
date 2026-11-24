//! Query plan trees captured from executed DataFusion physical plans.
//!
//! After a statement runs, every operator of its physical plan carries runtime
//! metrics (rows produced, compute time, ...). [`PlanNode`] snapshots that tree
//! so the UI can draw it without holding on to DataFusion types.

use std::sync::Arc;
use std::time::Duration;

use lancedb::datafusion::physical_plan::{ExecutionPlan, displayable};

/// One operator of an executed physical plan, with its runtime metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// Operator name, e.g. `ProjectionExec`.
    pub name: String,
    /// Operator parameters as DataFusion prints them (without the name).
    pub detail: String,
    /// Rows the operator emitted, when it reports them.
    pub output_rows: Option<usize>,
    /// CPU time spent inside the operator itself (excluding children).
    pub elapsed: Option<Duration>,
    /// Every metric the operator reported, as display-ready `(name, value)`.
    pub metrics: Vec<(String, String)>,
    /// Input operators.
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// Snapshots `plan` (and its inputs) including any collected metrics.
    pub fn from_execution_plan(plan: &Arc<dyn ExecutionPlan>) -> Self {
        let line = displayable(plan.as_ref()).one_line().to_string();
        let (name, detail) = split_label(line.trim(), plan.name());

        let (output_rows, elapsed, metrics) = match plan.metrics() {
            Some(set) => {
                let set = set
                    .aggregate_by_name()
                    .sorted_for_display()
                    .timestamps_removed();
                let metrics = set
                    .iter()
                    .map(|metric| {
                        let value = metric.value();
                        (value.name().to_string(), value.to_string())
                    })
                    .collect();
                let elapsed = set
                    .elapsed_compute()
                    .map(|nanos| Duration::from_nanos(nanos as u64));
                (set.output_rows(), elapsed, metrics)
            }
            None => (None, None, Vec::new()),
        };

        Self {
            name,
            detail,
            output_rows,
            elapsed,
            metrics,
            children: plan
                .children()
                .into_iter()
                .map(Self::from_execution_plan)
                .collect(),
        }
    }

    /// Number of operators in this subtree (including `self`).
    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(Self::node_count).sum::<usize>()
    }

    /// Largest per-operator compute time anywhere in this subtree.
    pub fn max_elapsed(&self) -> Duration {
        self.children
            .iter()
            .map(Self::max_elapsed)
            .fold(self.elapsed.unwrap_or_default(), Duration::max)
    }

    /// Sum of per-operator compute time in this subtree.
    pub fn total_elapsed(&self) -> Duration {
        self.elapsed.unwrap_or_default()
            + self
                .children
                .iter()
                .map(Self::total_elapsed)
                .sum::<Duration>()
    }
}

impl PlanNode {
    /// Parses DataFusion's indented `EXPLAIN ANALYZE` text, as a LanceDB
    /// Enterprise server returns it: one operator per line, two spaces of
    /// indentation per level, `Name: detail, metrics=[key=value, …]`.
    pub fn from_indent_text(text: &str) -> Option<Self> {
        let mut stack: Vec<(usize, PlanNode)> = Vec::new();
        let mut root = None;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let depth = (line.len() - line.trim_start().len()) / 2;
            while stack.last().is_some_and(|(open, _)| *open >= depth) {
                let (_, done) = stack.pop().expect("checked above");
                attach(&mut stack, &mut root, done);
            }
            stack.push((depth, Self::from_text_line(line.trim())));
        }
        while let Some((_, done)) = stack.pop() {
            attach(&mut stack, &mut root, done);
        }
        root
    }

    /// One operator line of an `EXPLAIN ANALYZE` plan.
    fn from_text_line(line: &str) -> Self {
        const METRICS: &str = ", metrics=[";
        let (head, metrics) = match line.rfind(METRICS) {
            Some(at) => {
                let inner = &line[at + METRICS.len()..];
                (&line[..at], inner.strip_suffix(']').unwrap_or(inner))
            }
            None => (line, ""),
        };
        let metrics: Vec<(String, String)> = metrics
            .split(", ")
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect();
        let metric = |key: &str| {
            metrics
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        let fallback = head.split([' ', ',']).next().unwrap_or(head);
        let (name, detail) = split_label(head, fallback);
        Self {
            name,
            detail,
            output_rows: metric("output_rows").and_then(parse_count),
            elapsed: metric("elapsed_compute").and_then(parse_duration),
            children: Vec::new(),
            metrics,
        }
    }
}

/// Adds a finished node to its parent (the top of `stack`), or makes it the
/// root.
fn attach(stack: &mut [(usize, PlanNode)], root: &mut Option<PlanNode>, node: PlanNode) {
    match stack.last_mut() {
        Some((_, parent)) => parent.children.push(node),
        None => {
            root.get_or_insert(node);
        }
    }
}

/// `100`, `100.0 K` or `2.3 M` (DataFusion's human-readable counts).
fn parse_count(text: &str) -> Option<usize> {
    let text = text.trim();
    let (number, scale) = match text.rsplit_once(' ') {
        Some((number, "K")) => (number, 1e3),
        Some((number, "M")) => (number, 1e6),
        Some((number, "B")) => (number, 1e9),
        Some((number, "T")) => (number, 1e12),
        _ => (text, 1.0),
    };
    let value: f64 = number.parse().ok()?;
    (value >= 0.0).then(|| (value * scale).round() as usize)
}

/// `42.42µs`, `77.04ms`, `1.2s` or `1ns` (Rust's `Duration` debug format).
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let seconds = match unit {
        "ns" => value / 1e9,
        "µs" | "us" => value / 1e6,
        "ms" => value / 1e3,
        "s" => value,
        _ => return None,
    };
    Some(Duration::from_secs_f64(seconds))
}

/// Splits DataFusion's one-line operator description into `(name, detail)`.
///
/// Most operators print as `NameExec: key=value, ...`; some print only their
/// name or use a different label than [`ExecutionPlan::name`].
fn split_label(line: &str, fallback_name: &str) -> (String, String) {
    match line.split_once(':') {
        Some((name, detail)) if !name.contains(' ') && !name.is_empty() => {
            (name.to_string(), detail.trim().to_string())
        }
        _ if line == fallback_name || line.is_empty() => (fallback_name.to_string(), String::new()),
        _ => (fallback_name.to_string(), line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str, ms: u64) -> PlanNode {
        PlanNode {
            name: name.to_string(),
            detail: String::new(),
            output_rows: Some(10),
            elapsed: Some(Duration::from_millis(ms)),
            metrics: vec![("output_rows".into(), "10".into())],
            children: Vec::new(),
        }
    }

    #[test]
    fn split_label_handles_common_shapes() {
        assert_eq!(
            split_label("ProjectionExec: expr=[a@0 as a]", "ProjectionExec"),
            ("ProjectionExec".into(), "expr=[a@0 as a]".into())
        );
        assert_eq!(
            split_label("CoalescePartitionsExec", "CoalescePartitionsExec"),
            ("CoalescePartitionsExec".into(), String::new())
        );
        assert_eq!(
            split_label("some free text", "Custom"),
            ("Custom".into(), "some free text".into())
        );
    }

    /// Shape of a plan a LanceDB Enterprise server returns for
    /// `EXPLAIN ANALYZE` (trimmed).
    const SERVER_PLAN: &str = "\
ProjectionExec: expr=[name@0 as name], metrics=[output_rows=100.0 K, elapsed_compute=42.42µs, output_bytes=2.3 MB]
  AggregateExec: mode=FinalPartitioned, gby=[name@0 as name], metrics=[output_rows=100.0 K, elapsed_compute=77.04ms]
    RepartitionExec: partitioning=Hash([name@0], 16), input_partitions=16, metrics=[output_rows=2.5 M, elapsed_compute=1.5s]
      TableIdentityExec, metrics=[]
        RemotePlanExec: partitions=1, workers=0, local_fallback_partitions=1, metrics=[output_rows=100, elapsed_compute=1ns]
          CoalescePartitionsExec: elapsed=11ms, metrics=[]
            LanceRead: uri=data, projection=[name]
    ExtraLeafExec
";

    #[test]
    fn parses_server_plan_text() {
        let root = PlanNode::from_indent_text(SERVER_PLAN).unwrap();
        assert_eq!(root.name, "ProjectionExec");
        assert_eq!(root.detail, "expr=[name@0 as name]");
        assert_eq!(root.output_rows, Some(100_000));
        assert_eq!(root.elapsed, Some(Duration::from_nanos(42_420)));
        assert_eq!(root.metrics.len(), 3);
        assert_eq!(root.node_count(), 8);

        let aggregate = &root.children[0];
        assert_eq!(
            aggregate.children.len(),
            2,
            "ExtraLeafExec is a second input"
        );
        let repartition = &aggregate.children[0];
        assert_eq!(repartition.output_rows, Some(2_500_000));
        assert_eq!(repartition.elapsed, Some(Duration::from_millis(1500)));
        let identity = &repartition.children[0];
        assert_eq!(
            (identity.name.as_str(), identity.detail.as_str()),
            ("TableIdentityExec", "")
        );
        let remote = &identity.children[0];
        assert!(
            remote
                .detail
                .contains("workers=0, local_fallback_partitions=1")
        );
        assert_eq!(remote.output_rows, Some(100));
        let read = &remote.children[0].children[0];
        assert_eq!(read.name, "LanceRead");
        assert!(read.metrics.is_empty() && read.output_rows.is_none());
        assert_eq!(aggregate.children[1].name, "ExtraLeafExec");

        assert_eq!(PlanNode::from_indent_text("  \n"), None);
    }

    #[test]
    fn parses_counts_and_durations() {
        assert_eq!(parse_count("7"), Some(7));
        assert_eq!(parse_count("1.5 B"), Some(1_500_000_000));
        assert_eq!(parse_count("2 T"), Some(2_000_000_000_000));
        assert_eq!(parse_count("x K"), None);
        assert_eq!(parse_count("-1"), None);
        assert_eq!(parse_duration("3us"), Some(Duration::from_micros(3)));
        assert_eq!(parse_duration("2s"), Some(Duration::from_secs(2)));
        assert_eq!(parse_duration("5 min"), None);
        assert_eq!(parse_duration("fast"), None);
        assert_eq!(parse_duration("12"), None);
    }

    #[test]
    fn aggregates_walk_the_whole_tree() {
        let mut root = leaf("Root", 1);
        root.children = vec![leaf("A", 5), leaf("B", 2)];
        assert_eq!(root.node_count(), 3);
        assert_eq!(root.max_elapsed(), Duration::from_millis(5));
        assert_eq!(root.total_elapsed(), Duration::from_millis(8));
    }
}
