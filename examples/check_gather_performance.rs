//! Bounded Arrow gather/export comparison. Run the same body against old/new
//! columnar modules; preparation and exhaustive value checks are outside timing.
#![allow(dead_code)]
#[path = "../src/aggregation.rs"]
mod aggregation;
#[path = "../src/arrow_types.rs"]
mod arrow_types;
#[path = "../src/columnar.rs"]
mod columnar;
#[path = "../src/datafusion_session.rs"]
mod datafusion_session;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/filtering.rs"]
mod filtering;
#[path = "../src/nested.rs"]
mod nested;
#[path = "../src/parquet_compat.rs"]
mod parquet_compat;
#[path = "../src/resource_budget.rs"]
mod resource_budget;
#[path = "../src/source_guard.rs"]
mod source_guard;
#[path = "../src/source_loader.rs"]
mod source_loader;
#[cfg(test)]
#[path = "../src/test_support.rs"]
mod test_support;

use anyhow::{Result, ensure};
use arrow::{
    array::{
        Array, ArrayRef, Int32Array, Int64Array, ListArray, StringArray, StructArray, UInt64Array,
    },
    datatypes::{Field, Int32Type},
    record_batch::RecordBatch,
};
use columnar::ArrowTable;
use serde_json::{Value, json};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    ALLOCATED.fetch_add(bytes, Ordering::Relaxed);
    CALLS.fetch_add(1, Ordering::Relaxed);
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            allocated(layout.size());
        }
        result
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe {
            System.dealloc(ptr, layout);
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, bytes: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, old, bytes) };
        if !result.is_null() {
            LIVE.fetch_sub(old.size(), Ordering::Relaxed);
            allocated(bytes);
        }
        result
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
enum Case {
    ContiguousExport,
    SingleBatchSparse,
    FlatSingleBatchSparse,
    CrossBatchFallback,
}
impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::ContiguousExport => "contiguous_export_8192",
            Self::SingleBatchSparse => "single_batch_sparse_duplicate_materialize",
            Self::FlatSingleBatchSparse => "flat_single_batch_sparse_duplicate_materialize",
            Self::CrossBatchFallback => "cross_batch_fallback_export_8192",
        }
    }
}

fn source(rows: usize) -> Result<(ArrowTable, RecordBatch)> {
    let ids = Arc::new(UInt64Array::from_iter_values(0..rows as u64)) as ArrayRef;
    let nullable = Arc::new(Int64Array::from_iter(
        (0..rows).map(|id| (id % 11 != 0).then_some(id as i64 * 3)),
    )) as ArrayRef;
    let text =
        Arc::new(StringArray::from_iter_values((0..rows).map(|id| {
            format!("row-{id:08}-abcdefghijklmnopqrstuvwxyz-0123456789")
        }))) as ArrayRef;
    let list = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
        (0..rows).map(|id| (id % 17 != 0).then_some(vec![Some((id % 31) as i32), None])),
    )) as ArrayRef;
    let nested = Arc::new(StructArray::try_new(
        vec![Arc::new(Field::new(
            "items",
            list.data_type().clone(),
            true,
        ))]
        .into(),
        vec![list],
        Some(arrow::buffer::NullBuffer::from_iter(
            (0..rows).map(|id| id % 19 != 0),
        )),
    )?) as ArrayRef;
    let batch = RecordBatch::try_from_iter([
        ("id", ids),
        ("nullable", nullable),
        ("text", text),
        ("nested", nested),
    ])?;
    let batches = (0..rows)
        .step_by(8192)
        .map(|first| batch.slice(first, (rows - first).min(8192)))
        .collect();
    let table = ArrowTable::new(arrow_types::columns_from_schema(&batch.schema()), batches)?;
    Ok((table, batch))
}

fn view(base: &ArrowTable, case: Case, rows: usize) -> Result<(ArrowTable, Vec<u64>)> {
    let order = match case {
        Case::ContiguousExport => (17..rows as u64 - 29).collect::<Vec<_>>(),
        Case::SingleBatchSparse | Case::FlatSingleBatchSparse => (0..rows)
            .map(|i| {
                let start = i / 8192 * 8192;
                let length = (rows - start).min(8192);
                (start
                    + if i % 13 == 0 {
                        0
                    } else {
                        (i % 8192 * 37) % length
                    }) as u64
            })
            .collect(),
        Case::CrossBatchFallback => (0..rows)
            .map(|i| {
                if i % 13 == 0 {
                    0
                } else {
                    (i as u64 * 104_729) % rows as u64
                }
            })
            .collect(),
    };
    let index = UInt64Array::from(order.clone());
    let chunks = if matches!(case, Case::SingleBatchSparse | Case::FlatSingleBatchSparse) {
        // Two streamed index chunks address each imported base batch.
        (0..rows)
            .step_by(8192)
            .flat_map(|first| {
                let length = (rows - first).min(8192);
                let cut = length / 3;
                [
                    index.slice(first, cut),
                    index.slice(first + cut, length - cut),
                ]
            })
            .collect()
    } else {
        (0..order.len())
            .step_by(32768)
            .map(|first| index.slice(first, (order.len() - first).min(32768)))
            .collect()
    };
    Ok((base.with_row_index_batches(chunks)?, order))
}

fn visit(
    data: &ArrowTable,
    case: Case,
    mut action: impl FnMut(RecordBatch) -> Result<()>,
) -> Result<()> {
    if matches!(case, Case::SingleBatchSparse | Case::FlatSingleBatchSparse) {
        for first in (0..data.row_count()).step_by(8192) {
            action(data.materialize_range(first..(first + 8192).min(data.row_count()))?)?;
        }
    } else {
        let cancel = AtomicBool::new(false);
        for batch in data.export_batches(8192, &cancel) {
            action(batch?)?;
        }
    }
    Ok(())
}

fn verify(data: &ArrowTable, case: Case, order: &[u64]) -> Result<()> {
    let mut position = 0;
    visit(data, case, |batch| {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let nullable = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let text = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let nested = (batch.num_columns() > 3).then(|| {
            batch
                .column(3)
                .as_any()
                .downcast_ref::<StructArray>()
                .unwrap()
        });
        for row in 0..batch.num_rows() {
            let id = order[position + row];
            ensure!(ids.value(row) == id, "row order mismatch");
            ensure!(
                nullable.is_null(row) == (id % 11 == 0),
                "scalar NULL mismatch"
            );
            if !nullable.is_null(row) {
                ensure!(nullable.value(row) == id as i64 * 3);
            }
            ensure!(
                text.value(row) == format!("row-{id:08}-abcdefghijklmnopqrstuvwxyz-0123456789")
            );
            if let Some(nested) = nested {
                ensure!(
                    nested.is_null(row) == (id % 19 == 0),
                    "Struct NULL mismatch"
                );
                if !nested.is_null(row) {
                    let lists = nested
                        .column(0)
                        .as_any()
                        .downcast_ref::<ListArray>()
                        .unwrap();
                    ensure!(lists.is_null(row) == (id % 17 == 0), "List NULL mismatch");
                    if !lists.is_null(row) {
                        let child = lists.value(row);
                        let values = child.as_any().downcast_ref::<Int32Array>().unwrap();
                        ensure!(
                            values.len() == 2
                                && values.value(0) == (id % 31) as i32
                                && values.is_null(1)
                        );
                    }
                }
            }
        }
        position += batch.num_rows();
        Ok(())
    })?;
    ensure!(position == order.len());
    Ok(())
}

fn measure(data: &ArrowTable, case: Case) -> Result<Value> {
    let allocated_before = ALLOCATED.load(Ordering::Relaxed);
    let calls_before = CALLS.load(Ordering::Relaxed);
    let live_before = LIVE.load(Ordering::Relaxed);
    PEAK.store(live_before, Ordering::Relaxed);
    let at = Instant::now();
    let mut rows = 0;
    let mut batches = 0;
    let mut maximum = 0;
    visit(data, case, |batch| {
        rows += batch.num_rows();
        maximum = maximum.max(batch.num_rows());
        batches += 1;
        black_box(&batch);
        Ok(())
    })?;
    let elapsed_ms = at.elapsed().as_secs_f64() * 1000.;
    let traffic = ALLOCATED.load(Ordering::Relaxed) - allocated_before;
    let calls = CALLS.load(Ordering::Relaxed) - calls_before;
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(live_before);
    Ok(
        json!({"elapsed_ms":elapsed_ms,"rows":rows,"batches":batches,"max_batch_rows":maximum,"rust_allocation_requested_bytes":traffic,"rust_allocation_calls":calls,"peak_additional_rust_live_bytes":peak}),
    )
}

fn main() -> Result<()> {
    let rows = std::env::var("PV_GATHER_BENCH_ROWS")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(262144usize);
    ensure!(rows >= 32768 && rows <= i32::MAX as usize);
    let (base, full) = source(rows)?;
    let source_bytes = full.get_array_memory_size();
    let mut cases = Vec::new();
    for case in [
        Case::ContiguousExport,
        Case::SingleBatchSparse,
        Case::FlatSingleBatchSparse,
        Case::CrossBatchFallback,
    ] {
        let input = if matches!(case, Case::FlatSingleBatchSparse) {
            base.project_columns(&[0, 1, 2])?
        } else {
            base.clone()
        };
        let (data, order) = view(&input, case, rows)?;
        let cold = measure(&data, case)?;
        verify(&data, case, &order)?;
        let mut warm = Vec::new();
        for _ in 0..5 {
            warm.push(measure(&data, case)?);
        }
        verify(&data, case, &order)?;
        cases.push(json!({"case":case.name(),"input_rows":rows,"result_rows":order.len(),"input_columns":input.schema().fields().len(),"index_logical_value_bytes":order.len()*8,"cold_metadata":cold,"warm":warm,"all_ids_order_scalar_nulls_text_nested_values_verified":true}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"source_rows":rows,"source_full_batch_array_memory_bytes":source_bytes,"base_batch_rows":8192,"streamed_index_chunk_rows":32768,"cases":cases,"scope":"Gather/export iterator only, no query or file serialization. Input/source/indices preparation and exhaustive value checks are outside timing. Cold means this view has not yet built export metadata; OS/CPU caches are uncontrolled. Rust counters track requested allocation traffic and additional simultaneously live allocations during each bounded iteration; they are not process RSS, physical footprint, Arrow payload totals, disk bytes or end-to-end export time. Source batches are shared slices of one generated batch; this avoids duplicating setup payload but makes Arrow get_array_memory_size of each slice unsuitable for summing physical storage."})
        )?
    );
    Ok(())
}
