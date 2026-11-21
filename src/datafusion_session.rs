//! The in-memory viewer's DataFusion function registry.
//!
//! The public filter protocol admits a small, pure scalar vocabulary, and the
//! aggregation editor uses a small set of aggregate implementations. Starting with an
//! empty registry avoids retaining DataFusion's unrelated file readers, window
//! functions and default function packages in the linked application. Parquet
//! decoding/encoding remains in our direct parquet crate, including its codecs.
use datafusion::{
    execution::{
        disk_manager::{DiskManagerBuilder, DiskManagerMode},
        memory_pool::GreedyMemoryPool,
        runtime_env::{RuntimeEnv, RuntimeEnvBuilder},
        session_state::SessionStateBuilder,
    },
    functions::{core, datetime, string, unicode},
    functions_aggregate::{
        approx_percentile_cont, average, count, min_max, percentile_cont,
        planner::AggregateFunctionPlanner, stddev, sum,
    },
    functions_nested::{
        array_has, length,
        planner::{FieldAccessPlanner, NestedFunctionPlanner},
    },
    prelude::{SessionConfig, SessionContext},
};
use std::sync::Arc;

pub(crate) fn memory_only_runtime(memory_bytes: usize) -> anyhow::Result<Arc<RuntimeEnv>> {
    query_runtime(memory_bytes, DiskManagerMode::Disabled)
}

pub(crate) fn lazy_spill_runtime(memory_bytes: usize) -> anyhow::Result<Arc<RuntimeEnv>> {
    // DataFusion creates a private directory only when a spill file is needed.
    query_runtime(memory_bytes, DiskManagerMode::OsTmpDirectory)
}

fn query_runtime(memory_bytes: usize, mode: DiskManagerMode) -> anyhow::Result<Arc<RuntimeEnv>> {
    Ok(Arc::new(
        RuntimeEnvBuilder::new()
            .with_memory_pool(Arc::new(GreedyMemoryPool::new(memory_bytes)))
            .with_disk_manager_builder(DiskManagerBuilder::default().with_mode(mode))
            .build()?,
    ))
}

pub(crate) fn new_context(config: SessionConfig, environment: Arc<RuntimeEnv>) -> SessionContext {
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_runtime_env(environment)
        .with_expr_planners(vec![
            Arc::new(core::planner::CoreFunctionPlanner::default()),
            Arc::new(NestedFunctionPlanner),
            Arc::new(FieldAccessPlanner),
            Arc::new(unicode::planner::UnicodeFunctionPlanner),
            Arc::new(AggregateFunctionPlanner),
        ])
        .with_scalar_functions(vec![
            core::arrow_cast(),
            core::get_field(),
            core::coalesce(),
            core::nullif(),
            datetime::date_part(),
            datetime::date_trunc(),
            string::lower(),
            string::upper(),
            string::starts_with(),
            string::ends_with(),
            string::contains(),
            string::btrim(),
            string::ltrim(),
            string::rtrim(),
            unicode::strpos(),
            // Registers the LENGTH and CHAR_LENGTH aliases as well.
            unicode::character_length(),
            array_has::array_has_udf(),
            length::array_length_udf(),
        ])
        .with_aggregate_functions(vec![
            count::count_udaf(),
            sum::sum_udaf(),
            average::avg_udaf(),
            min_max::min_udaf(),
            min_max::max_udaf(),
            stddev::stddev_udaf(),
            stddev::stddev_pop_udaf(),
            percentile_cont::percentile_cont_udaf(),
            approx_percentile_cont::approx_percentile_cont_udaf(),
        ])
        .build();
    SessionContext::new_with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use arrow::array::{BooleanArray, Decimal128Array};

    #[test]
    fn memory_only_queries_cannot_create_spill_files() -> Result<()> {
        let runtime = memory_only_runtime(16 * 1024 * 1024)?;
        assert!(matches!(
            runtime
                .disk_manager
                .create_tmp_file("testing an in-memory query"),
            Err(datafusion::error::DataFusionError::ResourcesExhausted(_))
        ));
        Ok(())
    }

    #[test]
    fn lean_session_keeps_filter_aliases_unicode_nested_and_exact_casts() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let context = new_context(SessionConfig::new(), Arc::new(RuntimeEnv::default()));
            let batches = context
                .sql(
                    "SELECT lower('AbC') a, upper('AbC') b, \
                     starts_with('AbC','A') c, ends_with('AbC','C') d, \
                     contains('上海A','海') e, strpos('上海A','海') f, \
                     length('上海😀') g, char_length('上海😀') h, trim('  hello ') i, \
                     coalesce(NULL,'ok') j, nullif(1,1) k, \
                     TRY_CAST('invalid' AS BIGINT) l, \
                     arrow_cast('27.123456789012345678','Decimal128(38, 18)') m, \
                     get_field({'a.b':27},'a.b') n, \
                     array_has(ARRAY[1,2],2) o, array_length(ARRAY[1,NULL,2]) p, \
                     arrow_cast('2024-02-29','Date32') q",
                )
                .await?
                .collect()
                .await?;
            let batch = &batches[0];
            let text = |column: usize| {
                arrow::util::display::array_value_to_string(batch.column(column).as_ref(), 0)
                    .unwrap()
            };
            assert_eq!(text(0), "abc");
            assert_eq!(text(1), "ABC");
            assert_eq!(text(8), "hello");
            assert_eq!(text(9), "ok");
            for column in [2, 3, 4, 14] {
                assert!(
                    batch
                        .column(column)
                        .as_any()
                        .downcast_ref::<BooleanArray>()
                        .unwrap()
                        .value(0)
                );
            }
            for (column, expected) in [(5, "2"), (6, "3"), (7, "3"), (15, "3")] {
                assert_eq!(text(column), expected);
            }
            assert!(batch.column(10).is_null(0));
            assert!(batch.column(11).is_null(0));
            assert_eq!(
                batch
                    .column(12)
                    .as_any()
                    .downcast_ref::<Decimal128Array>()
                    .unwrap()
                    .value(0),
                27_123_456_789_012_345_678
            );
            assert_eq!(text(13), "27");
            assert_eq!(
                batch.column(16).data_type(),
                &arrow::datatypes::DataType::Date32
            );
            Ok::<_, anyhow::Error>(())
        })
    }

    #[test]
    fn lean_session_keeps_grouped_count_distinct_and_null_aggregates() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let context = new_context(SessionConfig::new(), Arc::new(RuntimeEnv::default()));
            let batches = context
                .sql(
                    "SELECT g, COUNT(*) AS row_count, COUNT(x) present, COUNT(DISTINCT x) unique_values, \
                 SUM(x) total, AVG(x) mean, MIN(x) low, MAX(x) high \
                 FROM (VALUES ('a',1),('a',2),('a',2),('a',NULL),('b',NULL)) t(g,x) \
                 GROUP BY g ORDER BY g",
                )
                .await?
                .collect()
                .await?;
            let batch = &batches[0];
            assert_eq!(batch.num_rows(), 2);
            let text = |column: usize, row: usize| {
                arrow::util::display::array_value_to_string(batch.column(column).as_ref(), row)
                    .unwrap()
            };
            for (column,expected) in [(1,"4"),(2,"3"),(3,"2"),(4,"5"),(6,"1"),(7,"2")] {
                assert_eq!(text(column,0),expected);
            }
            assert_eq!(text(1, 1), "1");
            assert_eq!(text(2, 1), "0");
            assert_eq!(text(3, 1), "0");
            for column in [4, 5, 6, 7] {
                assert!(batch.column(column).is_null(1));
            }
            assert!(context.state().window_functions().is_empty());
            assert!(context.state().table_functions().is_empty());
            Ok::<_, anyhow::Error>(())
        })
    }
}
