//! Read logical CSV record boundaries and field counts without retaining the fields.
use super::{RECORD_BYTES, RECORD_SIZE_ERROR, check_cancel};
use anyhow::{Result, ensure};
use csv_core::{ReadRecordResult, Reader, ReaderBuilder};
use std::{
    io::{BufRead, BufReader, Read},
    sync::atomic::AtomicBool,
};

pub(super) struct Scanner<R> {
    input: BufReader<R>,
    parser: Reader,
    byte: u64,
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
            fields: [0; 4096],
            ends: [0; 128],
        }
    }

    pub(super) fn position(&self) -> u64 {
        self.byte
    }

    pub(super) fn next_record(&mut self, cancel: &AtomicBool) -> Result<Option<usize>> {
        let start = self.byte;
        let read_ahead = self.input.buffer().len() as u64;
        let mut field_count = 0;
        loop {
            check_cancel(cancel)?;
            let input = self.input.fill_buf()?;
            let (result, consumed, _, ends) =
                self.parser
                    .read_record(input, &mut self.fields, &mut self.ends);
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
