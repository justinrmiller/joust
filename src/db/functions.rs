//! LanceDB-specific SQL table functions.
//!
//! * `vector_search(table, column, vector, k [, metric])` runs a LanceDB
//!   nearest-neighbour query and exposes it to SQL with a `_distance` column.
//! * `fts(table, query_json)` is LanceDB's own full-text-search function, wired
//!   to joust's table registry.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use lancedb::DistanceType;
use lancedb::Table;
use lancedb::arrow::arrow_array::{Array, Float64Array};
use lancedb::arrow::arrow_cast::cast;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::datafusion::catalog::{Session, TableFunctionArgs, TableFunctionImpl, TableProvider};
use lancedb::datafusion::common::{DataFusionError, Result as DFResult, ScalarValue, plan_err};
use lancedb::datafusion::datasource::TableType;
use lancedb::datafusion::logical_expr::Expr;
use lancedb::datafusion::physical_expr::PhysicalExpr;
use lancedb::datafusion::physical_expr::expressions::Column;
use lancedb::datafusion::physical_plan::ExecutionPlan;
use lancedb::datafusion::physical_plan::projection::ProjectionExec;
use lancedb::index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase, QueryExecutionOptions};
use lancedb::table::datafusion::BaseTableAdapter;
use lancedb::table::datafusion::udtf::fts::TableResolver;

/// Usage string shown when `vector_search` is called incorrectly.
pub const VECTOR_SEARCH_USAGE: &str =
    "usage: vector_search('table', 'vector_column', '[0.1, 0.2, ...]', k [, 'l2'|'cosine'|'dot'])";

/// Name of the distance column LanceDB appends to vector search results.
pub const DISTANCE_COLUMN: &str = "_distance";

/// A LanceDB table together with what the SQL layer needs synchronously.
#[derive(Debug, Clone)]
pub struct RegisteredTable {
    pub table: Table,
    /// Table schema with metadata stripped (matches the DataFusion view).
    pub schema: SchemaRef,
    pub adapter: Arc<BaseTableAdapter>,
}

/// Shared name → table map consulted by the SQL table functions.
#[derive(Debug, Clone, Default)]
pub struct TableRegistry(Arc<RwLock<BTreeMap<String, RegisteredTable>>>);

impl TableRegistry {
    /// Replaces the registry contents.
    pub fn replace(&self, tables: BTreeMap<String, RegisteredTable>) {
        *self.0.write().unwrap_or_else(|poison| poison.into_inner()) = tables;
    }

    /// Looks up a table by exact name.
    pub fn get(&self, name: &str) -> Option<RegisteredTable> {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(name)
            .cloned()
    }

    /// Names of all registered tables, sorted.
    pub fn names(&self) -> Vec<String> {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Whether a LanceDB table with this exact name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    fn get_or_err(&self, name: &str) -> DFResult<RegisteredTable> {
        self.get(name).ok_or_else(|| {
            DataFusionError::Plan(format!("no LanceDB table named '{name}' is open"))
        })
    }
}

impl TableResolver for TableRegistry {
    fn resolve_table(
        &self,
        name: &str,
        fts_query: Option<FullTextSearchQuery>,
    ) -> DFResult<Arc<dyn TableProvider>> {
        let entry = self.get_or_err(name)?;
        Ok(match fts_query {
            Some(query) => Arc::new(entry.adapter.with_fts_query(query)),
            None => entry.adapter,
        })
    }
}

/// `vector_search(table, column, vector, k [, metric])`.
#[derive(Debug)]
pub struct VectorSearchFunction {
    registry: TableRegistry,
}

impl VectorSearchFunction {
    pub fn new(registry: TableRegistry) -> Self {
        Self { registry }
    }
}

impl TableFunctionImpl for VectorSearchFunction {
    fn call_with_args(&self, args: TableFunctionArgs) -> DFResult<Arc<dyn TableProvider>> {
        let exprs = args.exprs();
        if !(3..=5).contains(&exprs.len()) {
            return plan_err!("{VECTOR_SEARCH_USAGE}");
        }

        let table_name = string_arg(&exprs[0], "table")?;
        let column = string_arg(&exprs[1], "vector_column")?;
        let vector = vector_arg(&exprs[2])?;
        let k = match exprs.get(3) {
            Some(expr) => usize::try_from(int_arg(expr, "k")?)
                .ok()
                .filter(|k| *k > 0)
                .ok_or_else(|| DataFusionError::Plan("k must be a positive integer".into()))?,
            None => 10,
        };
        let metric = match exprs.get(4) {
            Some(expr) => parse_metric(&string_arg(expr, "metric")?)?,
            None => DistanceType::L2,
        };

        let entry = self.registry.get_or_err(&table_name)?;
        let Ok(field) = entry.schema.field_with_name(&column) else {
            return plan_err!("table '{table_name}' has no column '{column}'");
        };
        if !is_vector_type(field.data_type()) {
            return plan_err!(
                "column '{column}' is {} — vector_search needs a fixed-size list of floats",
                field.data_type()
            );
        }

        let mut fields: Vec<Field> = entry
            .schema
            .fields()
            .iter()
            .map(|field| field.as_ref().clone())
            .collect();
        fields.push(Field::new(DISTANCE_COLUMN, DataType::Float32, true));

        Ok(Arc::new(VectorSearchProvider {
            table: entry.table,
            schema: Arc::new(Schema::new(fields)),
            column,
            vector,
            k,
            metric,
        }))
    }
}

/// Table provider backed by a single LanceDB nearest-neighbour query.
#[derive(Debug)]
struct VectorSearchProvider {
    table: Table,
    schema: SchemaRef,
    column: String,
    vector: Vec<f32>,
    k: usize,
    metric: DistanceType,
}

#[async_trait]
impl TableProvider for VectorSearchProvider {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Temporary
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        let query = self
            .table
            .query()
            .nearest_to(self.vector.clone())
            .map_err(external)?
            .column(&self.column)
            .distance_type(self.metric)
            .limit(self.k);
        let plan = query
            .create_plan(QueryExecutionOptions::default())
            .await
            .map_err(external)?;

        // LanceDB decides the physical column order; re-project by name so the
        // plan matches exactly the (possibly pruned) schema DataFusion expects.
        let plan_schema = plan.schema();
        let wanted: Vec<usize> = match projection {
            Some(indices) => indices.clone(),
            None => (0..self.schema.fields().len()).collect(),
        };
        let exprs = wanted
            .iter()
            .map(|&index| {
                let name = self.schema.field(index).name();
                let position = plan_schema.index_of(name)?;
                let expr: Arc<dyn PhysicalExpr> = Arc::new(Column::new(name, position));
                Ok((expr, name.clone()))
            })
            .collect::<DFResult<Vec<_>>>()?;

        Ok(Arc::new(ProjectionExec::try_new(exprs, plan)?))
    }
}

/// Whether `data_type` can be searched with `vector_search`.
pub fn is_vector_type(data_type: &DataType) -> bool {
    match data_type {
        DataType::FixedSizeList(item, _) => matches!(
            item.data_type(),
            DataType::Float16 | DataType::Float32 | DataType::Float64 | DataType::UInt8
        ),
        _ => false,
    }
}

/// Parses a distance metric name as accepted by `vector_search`.
pub fn parse_metric(name: &str) -> DFResult<DistanceType> {
    match name.to_ascii_lowercase().as_str() {
        "l2" | "euclidean" => Ok(DistanceType::L2),
        "cosine" => Ok(DistanceType::Cosine),
        "dot" => Ok(DistanceType::Dot),
        "hamming" => Ok(DistanceType::Hamming),
        other => plan_err!("unknown distance metric '{other}' (expected l2, cosine or dot)"),
    }
}

/// Parses `"[1, 2.5, -3]"` (brackets optional) into a vector.
pub fn parse_vector_text(text: &str) -> Result<Vec<f32>, String> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let values = inner
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<f32>()
                .map_err(|_| format!("'{part}' is not a number"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() {
        Err("the query vector is empty".into())
    } else {
        Ok(values)
    }
}

fn string_arg(expr: &Expr, name: &str) -> DFResult<String> {
    match expr {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Ok(s.clone()),
        _ => plan_err!("argument '{name}' must be a string literal. {VECTOR_SEARCH_USAGE}"),
    }
}

fn int_arg(expr: &Expr, name: &str) -> DFResult<i64> {
    match expr {
        Expr::Literal(value, _) => match value {
            ScalarValue::Int8(Some(v)) => Ok((*v).into()),
            ScalarValue::Int16(Some(v)) => Ok((*v).into()),
            ScalarValue::Int32(Some(v)) => Ok((*v).into()),
            ScalarValue::Int64(Some(v)) => Ok(*v),
            ScalarValue::UInt8(Some(v)) => Ok((*v).into()),
            ScalarValue::UInt16(Some(v)) => Ok((*v).into()),
            ScalarValue::UInt32(Some(v)) => Ok((*v).into()),
            ScalarValue::UInt64(Some(v)) => i64::try_from(*v)
                .map_err(|_| DataFusionError::Plan(format!("argument '{name}' is too large"))),
            _ => plan_err!("argument '{name}' must be an integer literal"),
        },
        _ => plan_err!("argument '{name}' must be an integer literal"),
    }
}

/// Accepts the query vector as a string (`'[1, 2]'`) or an array literal.
/// DataFusion simplifies table-function arguments first, so `[1, -2]` and
/// `make_array(1, -2)` arrive here as list literals.
fn vector_arg(expr: &Expr) -> DFResult<Vec<f32>> {
    match expr {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => {
            parse_vector_text(s).map_err(DataFusionError::Plan)
        }
        Expr::Literal(ScalarValue::List(list), _) => list_values(list.values().as_ref()),
        Expr::Literal(ScalarValue::FixedSizeList(list), _) => list_values(list.values().as_ref()),
        _ => {
            plan_err!("the query vector must be a literal like '[0.1, 0.2]'. {VECTOR_SEARCH_USAGE}")
        }
    }
}

fn list_values(values: &dyn Array) -> DFResult<Vec<f32>> {
    let floats = cast(values, &DataType::Float64)?;
    let floats = floats
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| DataFusionError::Plan("vector values must be numbers".into()))?;
    floats
        .iter()
        .map(|v| {
            v.map(|v| v as f32).ok_or_else(|| {
                DataFusionError::Plan("vector values must be numbers, not NULL".into())
            })
        })
        .collect()
}

fn external(error: lancedb::Error) -> DataFusionError {
    DataFusionError::External(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vector_text() {
        assert_eq!(
            parse_vector_text("[1, 2.5,-3]").unwrap(),
            vec![1.0, 2.5, -3.0]
        );
        assert_eq!(parse_vector_text("0.5,0.25").unwrap(), vec![0.5, 0.25]);
        assert!(parse_vector_text("[]").is_err());
        assert!(parse_vector_text("[1, x]").is_err());
    }

    #[test]
    fn recognises_vector_columns() {
        let f32_item = Arc::new(Field::new("item", DataType::Float32, true));
        let utf8_item = Arc::new(Field::new("item", DataType::Utf8, true));
        assert!(is_vector_type(&DataType::FixedSizeList(f32_item, 8)));
        assert!(!is_vector_type(&DataType::FixedSizeList(utf8_item, 8)));
        assert!(!is_vector_type(&DataType::Float32));
    }

    #[test]
    fn parses_metrics() {
        assert_eq!(parse_metric("COSINE").unwrap(), DistanceType::Cosine);
        assert!(parse_metric("manhattan").is_err());
    }
}
