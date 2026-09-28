//! Scalar SQL functions for binary media columns.
//!
//! * `media_type(bytes)` → MIME type such as `image/png` (NULL if unknown)
//! * `image_width(bytes)`, `image_height(bytes)` → pixels (NULL if not an image)
//! * `byte_length(bytes)` → size in bytes (DataFusion's `octet_length` only
//!   accepts strings)

use std::sync::Arc;

use lancedb::arrow::arrow_array::{Int32Array, Int64Array, StringArray};
use lancedb::arrow::arrow_schema::DataType;
use lancedb::datafusion::common::Result as DFResult;
use lancedb::datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use lancedb::datafusion::prelude::SessionContext;

use crate::media;
use crate::results::binary_value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Which {
    MediaType,
    Width,
    Height,
    ByteLength,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct MediaFunction {
    which: Which,
    signature: Signature,
}

impl MediaFunction {
    fn new(which: Which) -> Self {
        Self {
            which,
            signature: Signature::uniform(
                1,
                vec![
                    DataType::Binary,
                    DataType::LargeBinary,
                    DataType::BinaryView,
                ],
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for MediaFunction {
    fn name(&self) -> &str {
        match self.which {
            Which::MediaType => "media_type",
            Which::Width => "image_width",
            Which::Height => "image_height",
            Which::ByteLength => "byte_length",
        }
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> DFResult<DataType> {
        Ok(match self.which {
            Which::MediaType => DataType::Utf8,
            Which::Width | Which::Height => DataType::Int32,
            Which::ByteLength => DataType::Int64,
        })
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DFResult<ColumnarValue> {
        let array = args.args[0].clone().into_array(args.number_rows)?;
        let rows = 0..array.len();
        let bytes = |row| binary_value(array.as_ref(), row);
        let result: lancedb::arrow::arrow_array::ArrayRef = match self.which {
            Which::ByteLength => Arc::new(
                rows.map(|row| bytes(row).map(|b| b.len() as i64))
                    .collect::<Int64Array>(),
            ),
            Which::MediaType => Arc::new(
                rows.map(|row| bytes(row).and_then(media::sniff).map(|f| f.mime))
                    .collect::<StringArray>(),
            ),
            Which::Width | Which::Height => Arc::new(
                rows.map(|row| {
                    let (w, h) = bytes(row).and_then(media::image_dimensions)?;
                    let side = if self.which == Which::Width { w } else { h };
                    i32::try_from(side).ok()
                })
                .collect::<Int32Array>(),
            ),
        };
        Ok(ColumnarValue::Array(result))
    }
}

/// Registers the media functions with a SQL session.
pub fn register(session: &SessionContext) {
    for which in [
        Which::MediaType,
        Which::Width,
        Which::Height,
        Which::ByteLength,
    ] {
        session.register_udf(ScalarUDF::from(MediaFunction::new(which)));
    }
}
