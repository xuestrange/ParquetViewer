use super::*;

/// Keep full-process peak memory for the index-only run independent of Arrow
/// page decoding. The Python driver runs these modes in separate processes.
pub(super) fn run(path: &Path, check_pages: bool) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let opened = Instant::now();
    let probe = CsvProbe::read_for_browse(path, &cancel)?;
    let sampled_rows = probe.sampled_records;
    let mut engine = Engine::open_prepared_csv(path, probe.prepared(), &cancel)?;
    let started = Instant::now();
    let (index, first_ms) = if check_pages {
        let (index, _, first) = engine.open_csv_view(&cancel)?;
        let first_ms = opened.elapsed().as_secs_f64() * 1000.;
        ensure!(first.row_count() > 0 && first.source_row_number(0) == Some(1));
        ensure!(first.cell(0, 0).text == "0");
        (index, Some(first_ms))
    } else {
        (engine.start_csv_index()?, None)
    };
    let progress = loop {
        let progress = index.progress();
        if progress.complete || progress.error.is_some() {
            break progress;
        }
        thread::sleep(Duration::from_millis(1));
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
    let mut jumps_ms = Vec::new();
    if check_pages {
        let resources = engine.resource_client();
        // Spread 21 single-row seeks across the verified prefix, in a
        // deterministic nonsequential order. Include the very last record.
        let last = progress
            .indexed_rows
            .checked_sub(1)
            .context("No indexed rows")?;
        for i in 0..21 {
            let row = last * ((13 * i) % 21) / 20;
            let jump = Instant::now();
            let page = index.read_projected_page(row, 1, 1024 * 1024, &[0], &resources, &cancel)?;
            jumps_ms.push(jump.elapsed().as_secs_f64() * 1000.);
            ensure!(page.source_row_number(0) == Some(row + 1));
            ensure!(page.display_text(0, 0) == row.to_string());
        }
    }
    println!(
        "{}",
        json!({
            "sampled_rows": sampled_rows,
            "index_ms": elapsed_ms,
            "first_typed_page_ms": first_ms,
            "jump_ms": jumps_ms,
            "indexed_bytes": progress.indexed_bytes,
            "indexed_rows": progress.indexed_rows,
            "complete": progress.complete,
            "error": progress.error,
        })
    );
    Ok(())
}
