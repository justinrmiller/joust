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

    #[test]
    fn aggregates_walk_the_whole_tree() {
        let mut root = leaf("Root", 1);
        root.children = vec![leaf("A", 5), leaf("B", 2)];
        assert_eq!(root.node_count(), 3);
        assert_eq!(root.max_elapsed(), Duration::from_millis(5));
        assert_eq!(root.total_elapsed(), Duration::from_millis(8));
    }
}
