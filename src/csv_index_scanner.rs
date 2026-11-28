//! Read logical CSV record boundaries and field counts without retaining the fields.
use super::{BUFFER_BYTES, RECORD_BYTES, RECORD_SIZE_ERROR, check_cancel};
use anyhow::{Result, ensure};
use csv_core::{ReadRecordResult, Reader, ReaderBuilder};
use memchr::{memchr, memchr_iter, memchr3};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    os::unix::fs::FileExt,
    sync::atomic::AtomicBool,
};

/// Each scanner reads from the same open file with an independent offset.
struct PositionedReader<'a> {
    file: &'a File,
    offset: u64,
    cache_start: u64,
    cached: &'a [u8],
}

impl Read for PositionedReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = if let Some(relative) = self.offset.checked_sub(self.cache_start)
            && relative < self.cached.len() as u64
        {
            let mut remaining = &self.cached[relative as usize..];
            remaining.read(bytes)?
        } else {
            // A record can cross the end of the cached wave. Keep reading the
            // file so only its actual end is presented to csv-core as EOF.
            self.file.read_at(bytes, self.offset)?
        };
        self.offset += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
pub(super) fn at(
    file: &File,
    delimiter: u8,
    seed: u64,
    offset: u64,
) -> Result<Scanner<impl Read + '_>> {
    at_cached(file, delimiter, seed, offset, 0, &[])
}

/// The cached span starts on a seed-relative input-buffer boundary and ends
/// on another such boundary or at EOF, preserving the original refill sizes.
pub(super) fn at_cached<'a>(
    file: &'a File,
    delimiter: u8,
    seed: u64,
    offset: u64,
    cache_start: u64,
    cached: &'a [u8],
) -> Result<Scanner<impl Read + 'a>> {
    // Recreate the sequential scanner's buffered bytes at this boundary. The
    // oversized-record check accounts for bytes already read before the row.
    let skip = ((offset - seed) % BUFFER_BYTES as u64) as usize;
    let mut input = BufReader::with_capacity(
        BUFFER_BYTES,
        PositionedReader {
            file,
            offset: offset - skip as u64,
            cache_start,
            cached,
        },
    );
    if skip > 0 {
        ensure!(
            input.fill_buf()?.len() >= skip,
            "Source changed after loading. Reload the file."
        );
        input.consume(skip);
    }
    Ok(Scanner::new(input, delimiter, offset))
}

/// A full, unquoted record can be indexed by counting delimiters in-place.
/// csv-core skips blank lines and records the boundary immediately after CR,
/// leaving LF in a CRLF pair for the next call.
fn plain_record(input: &[u8], delimiter: u8) -> Option<(usize, usize)> {
    let start = input.iter().position(|&b| b != b'\r' && b != b'\n')?;
    let row = &input[start..];
    let end = memchr3(b'\n', b'\r', b'"', row)?;
    if row[end] == b'"' {
        return None;
    }
    let fields = 1 + memchr_iter(delimiter, &row[..end]).count();
    Some((start + end + 1, fields))
}

/// Every ordinary byte after the first leaves the parser in the same state,
/// whether inside or outside quotes. A continued record can represent a long
/// run with one input byte, while its source offset advances by the full run.
fn ordinary_prefix(input: &[u8], delimiter: u8) -> usize {
    const MIN_RUN: usize = 256;
    if input.len() < MIN_RUN {
        return 0;
    }
    let (probe, tail) = input.split_at(MIN_RUN);
    if memchr3(delimiter, b'\n', b'\r', probe).is_some() || memchr(b'"', probe).is_some() {
        return 0;
    }
    let stop = memchr3(delimiter, b'\n', b'\r', tail).unwrap_or(tail.len());
    MIN_RUN + memchr(b'"', &tail[..stop]).unwrap_or(stop)
}

pub(super) struct Scanner<R> {
    input: BufReader<R>,
    parser: Reader,
    byte: u64,
    delimiter: u8,
    rows_before_probe: u8,
    // csv-core writes the unescaped bytes and field ends into caller-owned
    // slices. Reuse them even mid-record: indexing needs only the counts.
    fields: [u8; 4096],
    ends: [usize; 128],
}

impl<R: Read> Scanner<R> {
    pub(super) fn new(input: BufReader<R>, delimiter: u8, start_byte: u64) -> Self {
        let mut parser = ReaderBuilder::new().delimiter(delimiter).build();
        if start_byte != 0 {
            // csv-core strips a BOM on its first call. At a sample boundary,
            // U+FEFF is ordinary field data, even when followed by a quote.
            parser.read_record(b"\n", &mut [0], &mut [0]);
        }
        Self {
            input,
            parser,
            byte: start_byte,
            delimiter,
            // Let csv-core apply its original BOM handling at the file start.
            rows_before_probe: u8::from(start_byte == 0),
            fields: [0; 4096],
            ends: [0; 128],
        }
    }

    pub(super) fn position(&self) -> u64 {
        self.byte
    }

    pub(super) fn next_record(&mut self, cancel: &AtomicBool) -> Result<Option<usize>> {
        let start = self.byte;
        // The legacy reader grants each call 64 MiB of new I/O in addition
        // to bytes buffered while reading the previous row. At EOF the CSV
        // parser can consume that full allowance skipping trailing blank lines.
        let read_ahead = self.input.buffer().len() as u64;
        let mut field_count = 0;
        loop {
            check_cancel(cancel)?;
            let input = self.input.fill_buf()?;
            let mut run = 0;
            if self.byte == start {
                if self.rows_before_probe > 0 {
                    self.rows_before_probe -= 1;
                } else if let Some((bytes, fields)) = plain_record(input, self.delimiter) {
                    // csv-core remains at a record boundary, which is also the
                    // correct state for whichever record it next handles.
                    ensure!(bytes <= RECORD_BYTES, RECORD_SIZE_ERROR);
                    self.input.consume(bytes);
                    self.byte += bytes as u64;
                    return Ok(Some(fields));
                } else {
                    // Avoid probing every row of a mostly quoted file.
                    self.rows_before_probe = 32;
                }
            } else {
                run = ordinary_prefix(input, self.delimiter);
            }
            let source = if run == 0 { input } else { &input[..1] };
            let (result, consumed, _, ends) =
                self.parser
                    .read_record(source, &mut self.fields, &mut self.ends);
            debug_assert!(run == 0 || (consumed == 1 && ends == 0));
            let consumed = consumed.max(run);
            self.input.consume(consumed);
            self.byte += consumed as u64;
            let span = self.byte - start;
            if span > RECORD_BYTES as u64 {
                ensure!(
                    result != ReadRecordResult::Record && span - RECORD_BYTES as u64 <= read_ahead,
                    RECORD_SIZE_ERROR
                );
            }
            field_count += ends;
            match result {
                ReadRecordResult::Record => return Ok(Some(field_count)),
                ReadRecordResult::End => return Ok(None),
                ReadRecordResult::InputEmpty
                | ReadRecordResult::OutputFull
                | ReadRecordResult::OutputEndsFull => {}
            }
        }
    }
}
