use super::*;

fn source(path: &Path) -> Result<Arc<CsvSource>> {
    Ok(Arc::new(CsvSource::open(
        path,
        Some(CsvOptions {
            delimiter: b'|',
            infer_types: false,
            sample_records: 1,
            ..Default::default()
        }),
        &AtomicBool::new(false),
        |_, _| {},
    )?))
}

fn pool(workers: usize) -> Result<Arc<rayon::ThreadPool>> {
    Ok(Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()?,
    ))
}

fn finished(browse: &CsvBrowse) -> Result<(CsvIndexProgress, Vec<(usize, u64)>)> {
    let shared = &browse.index;
    let (state, _) = shared
        .changed
        .wait_timeout_while(
            shared.state.lock().unwrap(),
            Duration::from_secs(20),
            |state| !state.progress.complete && state.progress.error.is_none(),
        )
        .unwrap();
    assert!(
        state.progress.complete || state.progress.error.is_some(),
        "index did not finish"
    );
    let mut checkpoints = Vec::new();
    for n in 0..state.checkpoints {
        let checkpoint = shared.checkpoint(n)?;
        checkpoints.push((checkpoint.row, checkpoint.byte));
    }
    assert_eq!(shared.file.metadata()?.len(), state.checkpoints * 16);
    Ok((state.progress.clone(), checkpoints))
}

#[test]
fn partitioned_index_matches_serial_checkpoints_and_first_width_error() -> Result<()> {
    const BLOCK: u64 = 64 * 1024;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("partitioned.csv");
    let mut data = Vec::from(&b"id|text\r\n1|sample\r\n"[..]);
    let long_row = 16_000;
    for id in 2..long_row {
        writeln!(data, "{id}|leading rows\r")?;
    }
    write!(data, "{long_row}|\"")?;
    let text_end = data.len() + 2 * INDEX_BYTES as usize;
    // Every physical line in this field looks like a malformed three-field
    // record to a worker that guesses it begins outside quotes.
    while data.len() < text_end {
        data.extend_from_slice(b"first|second|third\r\n");
    }
    data.extend_from_slice(b"escaped \"\"quote\"\"\"\r\n");
    let rows = 2 * INDEX_ROWS + 17;
    for id in long_row + 1..=rows {
        write!(data, "{id}|")?;
        data.extend_from_slice(match id % 4 {
            0 => b"\r\n\r\n",
            1 => b"\"several\r\nlines\"\"with a quote\"\n",
            2 => b"literal\"quote\r",
            _ => b"plain\n",
        });
    }
    // A final unterminated row is complete only at the real end of the file.
    let good_prefix = data.len();
    for (tail, valid, expected_bytes) in [
        (&b"end|last"[..], true, good_prefix + 8),
        (&b"bad|width|here\nnext\n"[..], false, good_prefix),
    ] {
        data.truncate(good_prefix);
        data.extend_from_slice(tail);
        std::fs::write(&path, &data)?;
        let serial = source(&path)?.start_browse_with_pool(pool(1)?, BLOCK)?;
        let parallel = source(&path)?.start_browse_with_pool(pool(4)?, BLOCK)?;
        let (progress, checkpoints) = finished(&parallel)?;
        assert_eq!((progress.clone(), checkpoints.clone()), finished(&serial)?);
        assert!(checkpoints.len() >= 3);
        assert_eq!(progress.indexed_bytes, expected_bytes as u64);
        assert_eq!(progress.indexed_rows, rows + usize::from(valid));
        assert_eq!(progress.complete, valid);
        if valid {
            assert!(progress.error.is_none());
        } else {
            let error = progress.error.unwrap();
            assert!(
                error.contains(&format!("record {} has 3 fields, expected 2", rows + 1)),
                "{error}"
            );
        }
        let page =
            parallel.read_page(rows - 1, 2, 4096, &Client::new(), &AtomicBool::new(false))?;
        assert_eq!(page.source_row_number(0), Some(rows));
        assert_eq!(page.display_text(0, 0), rows.to_string());
        assert_eq!(page.row_count(), 1 + usize::from(valid));
        if valid {
            assert_eq!(page.display_text(1, 1), "last");
        }
    }
    Ok(())
}

#[test]
fn tiny_partitions_match_serial_csv_quoting_and_blank_lines() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed.csv");
    let fields: &[&[u8]] = &[
        b"",
        b"plain",
        b"12\" monitor",
        b"\"with|delimiter\"",
        b"\"closed\"suffix",
        b"\"escaped\"\"quote\"",
        b"\"embedded\r\nlines\n|pipes\"",
        b"\xef\xbb\xbf\"literal",
    ];
    let terminators: &[&[u8]] = &[b"\n", b"\r", b"\r\n", b"\r\n\n"];
    let mut seed = 19317_u32;
    let mut choose = |count| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) as usize % count
    };
    for _ in 0..4 {
        let mut data = Vec::from(&b"id|value\n1|sample\n"[..]);
        for id in 2..=512 {
            write!(data, "{id}|")?;
            data.extend_from_slice(fields[choose(fields.len())]);
            data.extend_from_slice(terminators[choose(terminators.len())]);
        }
        data.extend_from_slice(b"final|\"unterminated\nfield");
        std::fs::write(&path, data)?;
        let serial = source(&path)?.start_browse_with_pool(pool(1)?, 31)?;
        let expected = finished(&serial)?;
        assert!(expected.0.complete);
        assert_eq!(expected.0.indexed_rows, 513);
        for size in [13, 31, 127] {
            let parallel = source(&path)?.start_browse_with_pool(pool(3)?, size)?;
            assert_eq!(finished(&parallel)?, expected, "block size {size}");
        }
    }
    Ok(())
}

#[test]
fn first_page_and_cancellation_do_not_wait_for_parallel_index_workers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cancel.csv");
    let mut writer = BufWriter::new(File::create(&path)?);
    writer.write_all(b"id|label\n1|first\n")?;
    let block = b"2|next row\n".repeat(4096);
    for _ in 0..128 {
        writer.write_all(&block)?;
    }
    writer.flush()?;
    drop(writer);

    // Hold both workers so opening and reading the sampled row are independent
    // of timing or disk speed. Dropping the senders releases them on failure too.
    let workers = pool(2)?;
    let (ready, wait_ready) = std::sync::mpsc::channel();
    let mut release = Vec::new();
    let expired = Arc::new(AtomicBool::new(false));
    for _ in 0..workers.current_num_threads() {
        let (resume, wait) = std::sync::mpsc::channel::<()>();
        release.push(resume);
        let ready = ready.clone();
        let expired = expired.clone();
        workers.spawn(move || {
            let _ = ready.send(());
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                wait.recv_timeout(Duration::from_secs(20))
            {
                expired.store(true, Ordering::Release);
            }
        });
    }
    for _ in 0..workers.current_num_threads() {
        wait_ready.recv_timeout(Duration::from_secs(10))?;
    }

    let browse = source(&path)?.start_browse_with_pool(workers, 64 * 1024)?;
    let page = browse.read_page(0, 1, 4096, &Client::new(), &AtomicBool::new(false))?;
    assert!(
        !expired.load(Ordering::Acquire),
        "first page waited for the index"
    );
    assert_eq!(page.display_text(0, 1), "first");
    assert!(!browse.progress().complete);
    browse.cancel();
    drop(release);
    let (progress, _) = finished(&browse)?;
    assert!(!progress.complete);
    assert!(progress.error.unwrap().contains("cancelled"));
    Ok(())
}

#[test]
fn restarting_inside_a_buffer_preserves_the_trailing_blank_limit() -> Result<()> {
    let mut file = tempfile::tempfile()?;
    let seed = 13;
    let row = seed + BUFFER_BYTES as u64 + 4;
    file.seek(SeekFrom::Start(row))?;
    // A scanner arriving here from the sample has all but four bytes of
    // its current read buffer left. A fresh, empty buffer would reject this EOF.
    let buffered = BUFFER_BYTES as u64 - 4;
    let blanks = RECORD_BYTES as u64 + buffered;
    io::copy(&mut io::repeat(b'\n').take(blanks), &mut file)?;
    let cancel = AtomicBool::new(false);
    let mut resumed = scanner::at(&file, b'|', seed, row)?;
    assert_eq!(resumed.next_record(&cancel)?, None);
    assert_eq!(resumed.position(), row + blanks);
    drop(resumed);
    file.write_all(b"\n")?;
    let mut resumed = scanner::at(&file, b'|', seed, row)?;
    assert!(
        resumed
            .next_record(&cancel)
            .unwrap_err()
            .to_string()
            .contains("64 MiB")
    );
    Ok(())
}

#[test]
fn cached_wave_can_end_inside_a_multiline_record() -> Result<()> {
    let mut file = tempfile::tempfile()?;
    let header = b"id|text\n";
    file.write_all(header)?;
    let seed = header.len() as u64;
    let mut data = Vec::from(&b"1|first\n2|\"one\n"[..]);
    data.resize(2 * BUFFER_BYTES + 17, b'x');
    data.extend_from_slice(b"\r\ntwo\"\r\n3|last");
    file.write_all(&data)?;
    for (start, end) in [(0, BUFFER_BYTES), (BUFFER_BYTES, data.len())] {
        let mut cached = scanner::at_cached(
            &file,
            b'|',
            seed,
            seed,
            seed + start as u64,
            &data[start..end],
        )?;
        let mut reference = scanner::at(&file, b'|', seed, seed)?;
        let cancel = AtomicBool::new(false);
        for expected in [Some(2), Some(2), Some(2), None] {
            assert_eq!(cached.next_record(&cancel)?, expected);
            assert_eq!(reference.next_record(&cancel)?, expected);
            assert_eq!(cached.position(), reference.position());
        }
    }
    Ok(())
}
