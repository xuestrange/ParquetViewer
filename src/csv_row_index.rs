//! Incremental, on-disk logical-record checkpoints for a browsable text source.
use super::*;
use std::{ops::Range, os::unix::fs::FileExt, sync::Condvar, time::Duration};

#[derive(Clone)]
pub(crate) struct CsvBrowse {
    source: Arc<CsvSource>,
    index: Arc<SharedIndex>,
}

/// Published offsets are immutable. Readers take a snapshot of the frontier
/// before looking up checkpoints on their own file descriptor offsets.
#[derive(Clone)]
struct Published {
    checkpoints: u64,
    progress: CsvIndexProgress,
}

pub(super) struct SharedIndex {
    file: File,
    state: Mutex<Published>,
    changed: Condvar,
    cancelled: AtomicBool,
}

#[derive(Clone, Copy)]
struct Checkpoint {
    row: usize,
    byte: u64,
}

impl SharedIndex {
    fn append(&self, first: u64, anchors: &[Anchor]) -> Result<u64> {
        let mut number = first;
        // Bound the temporary storage even for a large explicit inference sample.
        for chunk in anchors.chunks(256) {
            let mut bytes = Vec::with_capacity(chunk.len() * 16);
            for anchor in chunk {
                bytes.extend_from_slice(&u64::try_from(anchor.row)?.to_le_bytes());
                bytes.extend_from_slice(&anchor.position.byte().to_le_bytes());
            }
            self.file.write_all_at(&bytes, number * 16)?;
            number += chunk.len() as u64;
        }
        Ok(number)
    }

    fn checkpoint(&self, number: u64) -> Result<Checkpoint> {
        let mut bytes = [0; 16];
        self.file.read_exact_at(&mut bytes, number * 16)?;
        Ok(Checkpoint {
            row: usize::try_from(u64::from_le_bytes(bytes[..8].try_into().unwrap()))?,
            byte: u64::from_le_bytes(bytes[8..].try_into().unwrap()),
        })
    }

    fn locate(&self, row: usize, count: u64) -> Result<(u64, Checkpoint)> {
        let (mut low, mut high) = (0, count);
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            if self.checkpoint(middle)?.row <= row {
                low = middle;
            } else {
                high = middle;
            }
        }
        Ok((low, self.checkpoint(low)?))
    }

    pub(super) fn progress(&self) -> CsvIndexProgress {
        self.state.lock().unwrap().progress.clone()
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn scan(
        &self,
        path: &Path,
        stamp: &Stamp,
        options: &CsvOptions,
        width: usize,
        mut index: Index,
    ) -> Result<()> {
        let mut reader = parser(
            RecordReader::new(stamp.open(path)?),
            options.delimiter,
            options.has_header,
            true,
        );
        reader.set_byte_headers(ByteRecord::new());
        reader.seek(index.next.position.clone())?;
        let mut scratch = RecordScratch::default();
        let mut checkpoints = self.state.lock().unwrap().checkpoints;
        while !index.complete {
            let begin = index.next.clone();
            let scan = (|| -> Result<()> {
                while index.next.row - begin.row < INDEX_ROWS
                    && index.next.position.byte() - begin.position.byte() < INDEX_BYTES
                    && CsvSource::advance_index(
                        stamp.len,
                        &mut index,
                        &mut reader,
                        &mut scratch,
                        width,
                        &self.cancelled,
                    )?
                {}
                Ok(())
            })();
            // Discard this wave when the file identity has changed. A malformed
            // record can still leave earlier, verified rows ready to browse.
            stamp.validate_path(path)?;
            checkpoints = self.append(checkpoints, &index.anchors[1..])?;
            if index.anchors.len() > 1 {
                let last = index.anchors.pop().unwrap();
                index.anchors.clear();
                index.anchors.push(last);
            }
            let mut state = self.state.lock().unwrap();
            state.checkpoints = checkpoints;
            state.progress.indexed_rows = index.next.row;
            state.progress.indexed_bytes = index.next.position.byte();
            state.progress.complete = index.complete;
            drop(state);
            self.changed.notify_all();
            scan?;
        }
        Ok(())
    }

    /// Explicit range loads and whole-source queries share the scanner's disk
    /// index. Keep planning memory fixed even for a query spanning the file.
    pub(super) fn foreach_batch<T: Send>(
        &self,
        source: &CsvSource,
        range: Range<usize>,
        plan: &DecodePlan<'_>,
        on_bytes: impl Fn(u64, u64),
        process: impl Fn(RecordBatch, usize) -> Result<Option<T>> + Sync,
    ) -> Result<Vec<T>> {
        let mut output = Vec::new();
        let mut start = range.start;
        while start < range.end {
            check_cancel(plan.cancel)?;
            let state = self.state.lock().unwrap();
            if start >= state.progress.indexed_rows && !state.progress.complete {
                if let Some(error) = &state.progress.error {
                    anyhow::bail!("{error}");
                }
                check_cancel(&self.cancelled)?;
                let (next, _) = self
                    .changed
                    .wait_timeout(state, Duration::from_millis(50))
                    .unwrap();
                let progress = next.progress.clone();
                drop(next);
                on_bytes(progress.indexed_bytes, progress.total_bytes);
                continue;
            }
            let published = state.clone();
            drop(state);
            let progress = &published.progress;
            on_bytes(progress.indexed_bytes, progress.total_bytes);
            let end = range.end.min(progress.indexed_rows);
            if start >= end {
                break;
            }
            let (mut number, mut current) = self.locate(start, published.checkpoints)?;
            let mut jobs = Vec::with_capacity(8);
            while start < end && jobs.len() < 8 {
                number += 1;
                let next = if number < published.checkpoints {
                    self.checkpoint(number)?
                } else {
                    Checkpoint {
                        row: progress.indexed_rows,
                        byte: progress.indexed_bytes,
                    }
                };
                ensure!(next.row > start, "Incomplete text record checkpoint");
                jobs.push(Job {
                    byte: current.byte,
                    end_byte: next.byte,
                    source_rows: next.row - current.row,
                    skip: start - current.row,
                    start,
                    rows: next.row.min(end) - start,
                });
                start = next.row.min(end);
                current = next;
            }
            output.extend(source.decode_jobs(&jobs, None, plan, &process)?);
        }
        Ok(output)
    }
}

impl CsvSource {
    pub(crate) fn start_browse(self: &Arc<Self>) -> Result<CsvBrowse> {
        let mut slot = self.browse_index.lock().unwrap();
        if let Some(index) = slot.as_ref() {
            return Ok(CsvBrowse {
                source: self.clone(),
                index: index.clone(),
            });
        }
        self.stamp.validate_path(&self.path)?;
        let initial = self.index.lock().unwrap();
        let index = Arc::new(SharedIndex {
            // Anonymous, removed automatically on close. No files are created
            // beside a user's source, even on an unclean application exit.
            file: tempfile::tempfile().context("Could not create the text record index")?,
            state: Mutex::new(Published {
                checkpoints: 0,
                progress: CsvIndexProgress {
                    indexed_rows: initial.next.row,
                    indexed_bytes: initial.next.position.byte(),
                    total_bytes: self.stamp.len,
                    complete: initial.complete,
                    error: None,
                },
            }),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
        });
        index.state.lock().unwrap().checkpoints = index.append(0, &initial.anchors)?;
        if !initial.complete {
            let worker = index.clone();
            let path = self.path.clone();
            let stamp = self.stamp.clone();
            let options = self.options.clone();
            let width = self.schema.fields().len();
            let seed = Index {
                anchors: vec![initial.anchors.last().unwrap().clone()],
                next: initial.next.clone(),
                complete: false,
            };
            std::thread::Builder::new()
                .name("csv-row-index".into())
                .spawn(move || {
                    if let Err(error) = worker.scan(&path, &stamp, &options, width, seed) {
                        worker.state.lock().unwrap().progress.error = Some(format!("{error:#}"));
                        worker.changed.notify_all();
                    }
                })?;
        }
        *slot = Some(index.clone());
        Ok(CsvBrowse {
            source: self.clone(),
            index,
        })
    }
}

impl Drop for CsvSource {
    fn drop(&mut self) {
        if let Some(index) = self.browse_index.get_mut().unwrap() {
            index.cancel();
        }
    }
}

impl CsvBrowse {
    pub(crate) fn progress(&self) -> CsvIndexProgress {
        self.index.progress()
    }

    pub(crate) fn cancel(&self) {
        self.index.cancel();
    }

    /// Decode only complete rows in the published prefix. The byte budget is
    /// soft for one oversized row so every valid logical row remains reachable.
    pub(crate) fn read_page(
        &self,
        start: usize,
        max_rows: usize,
        byte_budget: usize,
        resources: &Client,
        cancel: &AtomicBool,
    ) -> Result<ArrowTable> {
        let columns: Vec<_> = (0..self.source.schema.fields().len()).collect();
        self.read_projected_page(start, max_rows, byte_budget, &columns, resources, cancel)
    }

    /// Frame the same logical records, retaining typed buffers only for the
    /// requested source column ordinals, in the caller's order.
    pub(crate) fn read_projected_page(
        &self,
        start: usize,
        max_rows: usize,
        byte_budget: usize,
        columns: &[usize],
        resources: &Client,
        cancel: &AtomicBool,
    ) -> Result<ArrowTable> {
        check_cancel(cancel)?;
        ensure!(!columns.is_empty(), "Choose at least one field.");
        let schema = Arc::new(self.source.schema.project(columns)?);
        self.source.stamp.validate_path(&self.source.path)?;
        let published = self.index.state.lock().unwrap().clone();
        let end = start
            .saturating_add(max_rows)
            .min(published.progress.indexed_rows);
        let mut batches = Vec::new();
        if start < end {
            let (_, anchor) = self.index.locate(start, published.checkpoints)?;
            let source = &self.source;
            let file = source.stamp.open(&source.path)?;
            let mut reader = parser(
                RecordReader::new(file),
                source.options.delimiter,
                false,
                true,
            );
            reader.set_byte_headers(ByteRecord::new());
            let mut position = Position::new();
            position.set_byte(anchor.byte).set_record(anchor.row as u64);
            reader.seek(position)?;
            let mut scratch = RecordScratch::default();
            let mut row = anchor.row;
            let mut first_byte = anchor.byte;
            let mut end_byte = anchor.byte;
            // Empty fields and typed arrays also cost memory when a short row
            // contains many columns. Include per-cell overhead in page sizing.
            let row_overhead = columns.len().saturating_mul(24);
            while row < end {
                check_cancel(cancel)?;
                if row == start {
                    first_byte = reader.position().byte();
                }
                let record_byte = reader.position().byte();
                ensure!(
                    read_record(&mut reader, &mut scratch.record, record_byte)?,
                    "Incomplete text record checkpoint"
                );
                let byte = reader.position().byte();
                if row > start {
                    let overhead = row_overhead.saturating_mul(row - start + 1);
                    let bytes = byte
                        .saturating_sub(first_byte)
                        .saturating_add(overhead as u64);
                    if bytes > byte_budget as u64 {
                        break;
                    }
                }
                end_byte = byte;
                row += 1;
                scratch.trim();
                if row > start
                    && end_byte
                        .saturating_sub(first_byte)
                        .saturating_add(row_overhead.saturating_mul(row - start) as u64)
                        >= byte_budget as u64
                {
                    break;
                }
            }
            drop(reader);
            drop(scratch);
            let job = Job {
                byte: first_byte,
                end_byte,
                source_rows: row - start,
                skip: 0,
                start,
                rows: row - start,
            };
            let plan = DecodePlan {
                projection: columns,
                schema: &schema,
                cancel,
            };
            // The caller already runs off the UI thread. A single viewport
            // page has one decoder and needs no additional Rayon worker.
            source.decode_file_job(job, &plan, |batch, _| {
                batches.push(batch);
                Ok(())
            })?;
        }
        self.source.stamp.validate_path(&self.source.path)?;
        let metadata = columns
            .iter()
            .map(|&i| self.source.info.columns[i].clone())
            .collect();
        let table = ArrowTable::new_with_schema(metadata, batches, schema)?
            .with_source_row_start(start.saturating_add(1));
        publish(table, resources, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufWriter, Write};

    #[test]
    fn browse_scans_without_loading_and_reuses_disk_checkpoints_for_typed_queries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("index.tsv");
        let mut writer = BufWriter::new(File::create(&path)?);
        writeln!(writer, "id\ttext\r")?;
        let field = "x".repeat(1536);
        let rows = 3500;
        for id in 0..rows {
            writeln!(writer, "{id}\t\"{field}\r\n\"\"{id}\"\"\"\r")?;
        }
        writer.flush()?;
        drop(writer);
        let cancel = AtomicBool::new(false);
        let sample = CsvProbe::read_for_browse(&path, &cancel)?;
        assert!(sample.sampled_records > 1 && sample.sampled_records < DEFAULT_SAMPLE_RECORDS);
        let source = Arc::new(CsvSource::open_from_prepared(
            &path,
            sample.prepared(),
            &cancel,
        )?);
        let original_anchors = source.index.lock().unwrap().anchors.len();
        let browse = source.start_browse()?;
        let client = Client::new();
        let page = browse.read_page(0, 1000, 4500, &client, &cancel)?;
        assert!(
            browse
                .read_page(1, 1, 4500, &client, &AtomicBool::new(true))
                .is_err()
        );
        assert_eq!(page.row_count(), 2);
        assert_eq!(page.source_row_number(1), Some(2));
        assert_eq!(page.display_text(1, 1), format!("{field}\r\n\"1\""));
        let narrow = browse.read_page(1, 10, 1, &client, &cancel)?;
        assert_eq!(narrow.row_count(), 1);
        assert_eq!(narrow.display_text(0, 0), "1");
        let deep = source.load_window(
            &["id".into()],
            rows - 1,
            2,
            &client,
            &cancel,
            |_| {},
            |_, _| {},
        )?;
        assert_eq!(deep.row_count(), 1);
        assert_eq!(deep.source_row_number(0), Some(rows));
        assert_eq!(deep.display_text(0, 0), (rows - 1).to_string());
        let progress = browse.progress();
        assert!(progress.complete);
        assert_eq!(progress.indexed_bytes, progress.total_bytes);
        assert_eq!(progress.indexed_rows, rows);
        assert_eq!(source.index.lock().unwrap().anchors.len(), original_anchors);
        assert!(browse.index.file.metadata()?.len() >= 5 * 16);
        let deep = browse.read_page(rows - 1, 10, 4500, &client, &cancel)?;
        assert_eq!(deep.row_count(), 1);
        assert_eq!(
            deep.display_text(0, 1),
            format!("{field}\r\n\"{}\"", rows - 1)
        );
        let ids = browse.read_projected_page(rows - 1, 10, 4500, &[0], &client, &cancel)?;
        assert_eq!(ids.source_row_number(0), Some(rows));
        assert_eq!(ids.schema().fields().len(), 1);
        assert_eq!(ids.schema().field(0).data_type(), &DataType::Int64);
        assert_eq!(ids.display_text(0, 0), (rows - 1).to_string());
        assert!(ids.memory_size() < deep.memory_size());
        let reversed = browse.read_projected_page(rows - 1, 10, 4500, &[1, 0], &client, &cancel)?;
        assert_eq!(reversed.schema().field(0).name(), "text");
        assert_eq!(reversed.display_text(0, 0), deep.display_text(0, 1));
        assert_eq!(reversed.display_text(0, 1), ids.display_text(0, 0));
        let worker = browse.index.clone();
        drop(browse);
        drop(source);
        assert!(worker.cancelled.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn projecting_a_valid_column_bypasses_an_unrelated_late_type_error() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("late.csv");
        std::fs::write(&path, "id,score\n1,20\n2,unavailable\n")?;
        let cancel = AtomicBool::new(false);
        let source = Arc::new(CsvSource::open(
            &path,
            Some(CsvOptions {
                sample_records: 1,
                ..Default::default()
            }),
            &cancel,
            |_, _| {},
        )?);
        let browse = source.start_browse()?;
        let client = Client::new();
        source.load_window(&["id".into()], 1, 1, &client, &cancel, |_| {}, |_, _| {})?;
        assert!(browse.read_page(1, 1, 1024, &client, &cancel).is_err());
        let id = browse.read_projected_page(1, 1, 1024, &[0], &client, &cancel)?;
        assert_eq!(id.source_row_number(0), Some(2));
        assert_eq!(id.display_text(0, 0), "2");
        Ok(())
    }

    #[test]
    fn browse_retains_valid_prefix_after_index_error_and_rejects_replaced_input() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("width.csv");
        std::fs::write(
            &path,
            "value,other\r\n1,0011\r\n2,0012\r\n3,0013\r\n4,wrong,width\r\n",
        )?;
        let cancel = AtomicBool::new(false);
        let sample = CsvProbe::read_for_browse(&path, &cancel)?;
        assert_eq!(sample.sampled_records, 3);
        let source = Arc::new(CsvSource::open_from_prepared(
            &path,
            sample.prepared(),
            &cancel,
        )?);
        let browse = source.start_browse()?;
        let client = Client::new();
        let failure = source
            .load_window(&[], 5, 1, &client, &cancel, |_| {}, |_, _| {})
            .err()
            .unwrap();
        assert!(failure.to_string().contains("record 4"), "{failure}");
        let progress = browse.progress();
        assert_eq!(progress.indexed_rows, 3);
        assert!(!progress.complete && progress.error.is_some());
        let page = browse.read_page(2, 4, 4096, &client, &cancel)?;
        assert_eq!(page.source_row_number(0), Some(3));
        assert_eq!(page.display_text(0, 1), "0013");
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "value,other\r\n9,test\r\n")?;
        std::fs::rename(replacement, &path)?;
        assert!(browse.read_page(0, 1, 4096, &client, &cancel).is_err());
        Ok(())
    }

    #[test]
    fn browse_preserves_headerless_bom_and_final_unterminated_record() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raw.csv");
        std::fs::write(
            &path,
            "\u{feff}first,01\r\n\u{feff}second,\"line 1\r\nline 2\"",
        )?;
        let cancel = AtomicBool::new(false);
        let source = Arc::new(CsvSource::open(
            &path,
            Some(CsvOptions {
                has_header: false,
                infer_types: false,
                sample_records: 1,
                ..Default::default()
            }),
            &cancel,
            |_, _| {},
        )?);
        let client = Client::new();
        let browse = source.start_browse()?;
        let first = browse.read_page(0, 1, 32, &client, &cancel)?;
        assert_eq!(first.display_text(0, 0), "first");
        let typed = source.load_window(&[], 1, 2, &client, &cancel, |_| {}, |_, _| {})?;
        let last = browse.read_page(1, 2, 32, &client, &cancel)?;
        for page in [typed, last] {
            assert_eq!(page.row_count(), 1);
            assert_eq!(page.source_row_number(0), Some(2));
            assert_eq!(page.display_text(0, 0), "\u{feff}second");
            assert_eq!(page.display_text(0, 1), "line 1\r\nline 2");
        }
        let progress = browse.progress();
        assert!(progress.complete);
        assert_eq!(progress.indexed_bytes, progress.total_bytes);
        assert_eq!(progress.indexed_rows, 2);
        Ok(())
    }

    #[test]
    fn browse_opens_header_only_file_and_checks_identity_before_starting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("header.csv");
        std::fs::write(&path, "id,value")?;
        let cancel = AtomicBool::new(false);
        let open = || -> Result<_> {
            Ok(Arc::new(CsvSource::open_from_prepared(
                &path,
                CsvProbe::read_for_browse(&path, &cancel)?.prepared(),
                &cancel,
            )?))
        };
        let fresh = open()?;
        let browse = fresh.start_browse()?;
        let progress = browse.progress();
        assert!(progress.complete);
        assert_eq!(progress.indexed_rows, 0);
        assert_eq!(progress.indexed_bytes, progress.total_bytes);
        let page = browse.read_page(0, 30, 512, &Client::new(), &cancel)?;
        assert_eq!(page.row_count(), 0);
        assert_eq!(page.schema().fields().len(), 2);
        let old = open()?;
        std::fs::rename(&path, dir.path().join("old.csv"))?;
        std::fs::write(&path, "id,value")?;
        assert!(old.start_browse().is_err());
        Ok(())
    }

    #[test]
    fn browse_stops_at_oversized_record_and_keeps_previous_page() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("huge.csv");
        let mut file = File::create(&path)?;
        let prefix = b"id,value\n1,first\n2,second\n";
        file.write_all(prefix)?;
        file.write_all(b"3,")?;
        file.seek(SeekFrom::Current(RECORD_BYTES as i64))?;
        file.write_all(b"\n")?;
        drop(file);
        let cancel = AtomicBool::new(false);
        let source = Arc::new(CsvSource::open(
            &path,
            Some(CsvOptions {
                sample_records: 1,
                ..Default::default()
            }),
            &cancel,
            |_, _| {},
        )?);
        let browse = source.start_browse()?;
        let client = Client::new();
        let error = source
            .load_window(&[], 2, 1, &client, &cancel, |_| {}, |_, _| {})
            .unwrap_err();
        assert!(format!("{error:#}").contains("64 MiB"), "{error:#}");
        assert_eq!(browse.progress().indexed_rows, 2);
        assert_eq!(
            browse
                .read_page(0, 2, 1024, &client, &cancel)?
                .display_text(0, 1),
            "first"
        );
        drop(browse);
        drop(source);
        // Seek directly to a final, unterminated record of exactly 64 MiB.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len((prefix.len() + RECORD_BYTES) as u64)?;
        let source = Arc::new(CsvSource::open(
            &path,
            Some(CsvOptions {
                sample_records: 2,
                ..Default::default()
            }),
            &cancel,
            |_, _| {},
        )?);
        let browse = source.start_browse()?;
        let empty = source.load_window(&[], 3, 1, &client, &cancel, |_| {}, |_, _| {})?;
        assert_eq!(empty.row_count(), 0);
        assert!(browse.progress().complete);
        assert_eq!(browse.progress().indexed_rows, 3);
        Ok(())
    }
}
