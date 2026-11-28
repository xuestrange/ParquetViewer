use super::*;
use std::io::{BufWriter, Write};

/// Compare every returned offset, including the one after trailing blank lines.
fn compare_records(data: &[u8], delimiter: u8, chunk: usize) -> Result<()> {
    let mut reference = ReaderBuilder::new()
        .buffer_capacity(chunk)
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(data);
    let mut scanner = scanner::Scanner::new(BufReader::with_capacity(chunk, data), delimiter, 0);
    let cancel = AtomicBool::new(false);
    let mut record = ByteRecord::new();
    loop {
        let present = reference.read_byte_record(&mut record)?;
        let before = scanner.position();
        assert_eq!(
            scanner.next_record(&cancel)?,
            present.then(|| record.len()),
            "field count at offset {before}, delimiter {delimiter}, buffer {chunk}"
        );
        assert_eq!(
            scanner.position(),
            reference.position().byte(),
            "next offset after {before}, delimiter {delimiter}, buffer {chunk}"
        );
        if !present {
            return Ok(());
        }
    }
}

#[test]
fn record_boundaries_and_widths_match_csv_across_buffer_splits() -> Result<()> {
    // Both ordinary records and arbitrary byte sequences exercise csv's
    // permissive handling of quotes in unquoted fields and after closing quotes.
    for delimiter in [b',', b'\t', b'|', b';', b' '] {
        let mut data = Vec::from(&b"\xef\xbb\xbf\"a\r\nb\""[..]);
        data.push(delimiter);
        data.extend_from_slice(b"12\" monitor\r\n\r\n");
        let alphabet = [b'x', b'"', b'"', b'\r', b'\n', delimiter, 0, 0xff];
        let mut state = 92821u32;
        for _ in 0..8192 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push(alphabet[(state >> 29) as usize]);
        }
        for chunk in [1, 2, 3, 7, 16, 63, 64, 65, 257, BUFFER_BYTES] {
            compare_records(&data, delimiter, chunk)?;
            compare_records(b"\r\n\r\n", delimiter, chunk)?;
            compare_records(b"\"final\nrow", delimiter, chunk)?;
        }
    }
    // Exercise records spanning many output buffers and input refills.
    let mut data = vec![b','; 16_384];
    data.extend_from_slice(b"\r\n\"");
    data.extend(std::iter::repeat_n(b'x', 3 * BUFFER_BYTES));
    data.extend_from_slice(b"\n\"\"end\"\"\"");
    compare_records(&data, b',', 251)
}

#[test]
fn unquoted_rows_and_long_runs_match_csv_through_state_transitions() -> Result<()> {
    for delimiter in [b',', b'\t', b'x'] {
        let mut data = Vec::new();
        for n in 0..200 {
            for width in [1, 2, 3, 96] {
                data.push(b'0');
                data.extend(std::iter::repeat_n(delimiter, width - 1));
                data.push(b'\n');
            }
            if n % 31 == 0 {
                data.extend_from_slice(b"\r\n\n\"a\"\"b\r\nc\"\r");
            }
        }
        // Exercise a long run at field start, in an unquoted field, inside
        // quotes, and after a closing quote. Finish at structural bytes or EOF.
        for length in [256, 1_025, 4_096, 65_537] {
            for prefix in [b"" as &[u8], b"plain", b"\"", b"\"closed\""] {
                for suffix in [b'"', delimiter, b'\r', b'\n'] {
                    data.extend_from_slice(prefix);
                    data.extend(std::iter::repeat_n(b'a', length));
                    data.extend_from_slice(&[suffix, delimiter, b'\n']);
                }
            }
        }
        data.extend_from_slice(b"\r\n");
        data.extend(std::iter::repeat_n(b'z', 4097));
        for chunk in [1_024, 4_096, BUFFER_BYTES] {
            compare_records(&data, delimiter, chunk)?;
        }
    }
    Ok(())
}

#[test]
fn trailing_blank_lines_respect_legacy_read_ahead_at_the_record_limit() -> Result<()> {
    let cancel = AtomicBool::new(false);
    let mut first_buffer = vec![b'\n'; BUFFER_BYTES];
    first_buffer[..4].copy_from_slice(b"a,b\n");
    // The legacy reader resets its input allowance after "a,b\n". Bytes it
    // already buffered can lead it to EOF without producing another record.
    for (extra, last, accepted) in [
        (1024, b"" as &[u8], true),
        (BUFFER_BYTES, b"" as &[u8], false),
        (1024, b"x\n" as &[u8], false),
    ] {
        let tail = 4 + RECORD_BYTES + extra - BUFFER_BYTES;
        let input = first_buffer
            .as_slice()
            .chain(io::repeat(b'\n').take(tail as u64))
            .chain(last);
        let mut scanner =
            scanner::Scanner::new(BufReader::with_capacity(BUFFER_BYTES, input), b',', 0);
        assert_eq!(scanner.next_record(&cancel)?, Some(2));
        if accepted {
            assert_eq!(scanner.next_record(&cancel)?, None);
            assert_eq!(scanner.position(), (4 + RECORD_BYTES + extra) as u64);
        } else {
            assert!(
                scanner
                    .next_record(&cancel)
                    .unwrap_err()
                    .to_string()
                    .contains("64 MiB")
            );
        }
    }
    Ok(())
}

#[test]
fn literal_bom_before_quote_after_sample_and_at_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("boundaries.csv");
    let mut out = BufWriter::new(File::create(&path)?);
    writeln!(out, "name,id")?;
    for i in 0..INDEX_ROWS + 3 {
        let name = if i == 1 || i == INDEX_ROWS {
            "\u{feff}\"literal"
        } else {
            "value"
        };
        writeln!(out, "{name},{i}")?;
    }
    out.flush()?;
    drop(out);
    let cancel = AtomicBool::new(false);
    let source = Arc::new(CsvSource::open(
        &path,
        Some(CsvOptions {
            sample_records: 1,
            infer_types: false,
            ..Default::default()
        }),
        &cancel,
        |_, _| {},
    )?);
    let browse = source.start_browse()?;
    let client = Client::new();
    source.load_window(&[], INDEX_ROWS + 2, 1, &client, &cancel, |_| {}, |_, _| {})?;
    assert_eq!(browse.progress().indexed_rows, INDEX_ROWS + 3);
    for i in [1, INDEX_ROWS] {
        let page = browse.read_page(i, 2, 4096, &client, &cancel)?;
        assert_eq!(page.row_count(), 2);
        assert_eq!(page.source_row_number(0), Some(i + 1));
        assert_eq!(page.display_text(0, 0), "\u{feff}\"literal");
        assert_eq!(page.display_text(0, 1), i.to_string());
    }
    Ok(())
}

#[test]
fn error_publishes_all_verified_checkpoints_and_exact_byte_frontier() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("checkpoints.csv");
    let header = b"id,label\n";
    let record = b"1,x\n";
    let valid = (CHECKPOINT_BATCH + 3) * INDEX_ROWS + 5;
    let mut out = BufWriter::new(File::create(&path)?);
    out.write_all(header)?;
    for _ in 0..valid {
        out.write_all(record)?;
    }
    out.write_all(b"unexpected,width,here\n")?;
    out.flush()?;
    drop(out);

    let cancel = AtomicBool::new(false);
    let source = Arc::new(CsvSource::open(&path, None, &cancel, |_, _| {})?);
    let browse = source.start_browse()?;
    let error = source
        .load_window(&[], valid, 1, &Client::new(), &cancel, |_| {}, |_, _| {})
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("record {} has 3 fields", valid + 1))
    );
    let state = browse.index.state.lock().unwrap().clone();
    assert_eq!(state.progress.indexed_rows, valid);
    assert_eq!(
        state.progress.indexed_bytes,
        (header.len() + valid * record.len()) as u64
    );
    assert_eq!(state.checkpoints, (1 + valid / INDEX_ROWS) as u64);
    assert_eq!(browse.index.file.metadata()?.len(), state.checkpoints * 16);
    for i in 0..state.checkpoints {
        let checkpoint = browse.index.checkpoint(i)?;
        let row = i as usize * INDEX_ROWS;
        assert_eq!(checkpoint.row, row);
        assert_eq!(checkpoint.byte, (header.len() + row * record.len()) as u64);
    }
    Ok(())
}
