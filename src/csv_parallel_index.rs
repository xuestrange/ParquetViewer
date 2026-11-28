//! Parallel logical-record indexing with source-ordered boundary verification.
use super::*;
use memchr::memchr2;
use std::sync::LazyLock;

pub(super) const BLOCK_BYTES: u64 = INDEX_BYTES;

// All open delimited files share this pool. Its creation and any wait for a
// worker happen on the background coordinator, outside the first-page path.
static POOL: LazyLock<Option<Arc<rayon::ThreadPool>>> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_cpus::get_physical().max(1))
        .thread_name(|n| format!("csv-index-{n}"))
        .build()
        .ok()
        .map(Arc::new)
});

#[cfg(test)]
#[test]
fn default_worker_count_matches_this_computer() {
    let pool = POOL.as_ref().expect("create the shared CSV index pool");
    assert_eq!(pool.current_num_threads(), num_cpus::get_physical());
}

pub(super) fn run(
    shared: &SharedIndex,
    path: &Path,
    stamp: &Stamp,
    options: &CsvOptions,
    width: usize,
    mut index: Index,
    pool: Option<Arc<rayon::ThreadPool>>,
    block_bytes: u64,
) -> Result<()> {
    let seed = index.next.position.byte();
    if stamp.len.saturating_sub(seed) <= block_bytes {
        return shared.scan(path, stamp, options, width, index);
    }
    let Some(pool) = pool.or_else(|| POOL.clone()) else {
        return shared.scan(path, stamp, options, width, index);
    };
    if pool.current_num_threads() == 1 {
        return pool.install(|| shared.scan(path, stamp, options, width, index));
    }
    let file = stamp.open(path)?;
    let scan = Scan {
        file: &file,
        seed,
        delimiter: options.delimiter,
        width,
        cancel: &shared.cancelled,
        cache_start: seed,
        bytes: &[],
    };
    let result = pool.install(|| -> Result<()> {
        // Publish a small, verified prefix even if another worker encounters
        // a long multiline record in the first parallel wave.
        let initial_end = seed
            .saturating_add(block_bytes.min(BUFFER_BYTES as u64))
            .min(stamp.len);
        scan.advance(&mut index, initial_end, stamp.len)?;
        shared.publish(path, stamp, &mut index)?;
        let mut stream = &file;
        let mut wave = Wave::default();
        while !index.complete {
            check_cancel(scan.cancel)?;
            let base = index.next.position.byte();
            let slots = (stamp.len - base)
                .div_ceil(block_bytes)
                .min(pool.current_num_threads() as u64) as usize;
            let end = base
                .saturating_add(slots as u64 * block_bytes)
                .min(stamp.len);
            if wave.fill(&mut stream, seed, base, end, stamp.len).is_err() {
                // A speculative bulk read can reach an unreadable region
                // before a valid prefix or an earlier malformed row.
                scan.advance(
                    &mut index,
                    base.saturating_add(block_bytes).min(end),
                    stamp.len,
                )?;
                shared.publish(path, stamp, &mut index)?;
                continue;
            }
            let scan = Scan {
                cache_start: wave.start,
                bytes: &wave.bytes,
                ..scan
            };
            let blocks: Vec<_> = (0..slots)
                .into_par_iter()
                .map(|i| {
                    let begin = base + i as u64 * block_bytes;
                    let end = begin.saturating_add(block_bytes).min(stamp.len);
                    let start = if i == 0 {
                        Some(begin)
                    } else {
                        scan.guess(begin, end)
                    };
                    // An unreadable candidate offset is inconclusive. Retry
                    // from the trusted prefix and report its first real error.
                    let chunk = start.and_then(|start| scan.chunk(start, end, i != 0).ok());
                    (end, chunk)
                })
                .collect();

            for (end, candidate) in blocks {
                if index.complete || index.next.position.byte() >= end {
                    continue;
                }
                check_cancel(scan.cancel)?;
                if let Some(chunk) = candidate {
                    // Only offsets reached from the known prefix authorize a
                    // worker's rows, width errors, or EOF. A physical newline
                    // may have been inside a quoted field on either side.
                    scan.advance(&mut index, chunk.start.min(end), stamp.len)?;
                    if index.complete {
                        break;
                    }
                    if let Some(skip) = chunk.rows_before(index.next.position.byte()) {
                        chunk.apply(skip, &mut index, stamp.len)?;
                        continue;
                    }
                }
                scan.advance(&mut index, end, stamp.len)?;
            }
            shared.publish(path, stamp, &mut index)?;
        }
        Ok(())
    });
    // Preserve all records before the first bad row, but discard unpublished
    // results if the source identity changed while workers were reading.
    if result.is_err() {
        shared.publish(path, stamp, &mut index)?;
    }
    result
}

#[derive(Default)]
struct Wave {
    start: u64,
    bytes: Vec<u8>,
}

impl Wave {
    /// Read consecutive ranges through a single file cursor so storage can
    /// read ahead. Retain overlapping bytes for the next worker wave. The
    /// aligned padding supplies a full refill on either side of a cut.
    fn fill(
        &mut self,
        stream: &mut (impl Read + Seek),
        seed: u64,
        base: u64,
        target: u64,
        length: u64,
    ) -> Result<()> {
        let buffer = BUFFER_BYTES as u64;
        let start = base - (base - seed) % buffer;
        let end = (target - (target - seed) % buffer)
            .saturating_add(2 * buffer)
            .min(length);
        let previous_end = self.start + self.bytes.len() as u64;
        let keep = if !self.bytes.is_empty()
            && start >= self.start
            && start <= previous_end
            && previous_end <= end
        {
            let keep = (previous_end - start) as usize;
            self.bytes.copy_within((start - self.start) as usize.., 0);
            keep
        } else {
            stream.seek(SeekFrom::Start(start))?;
            0
        };
        self.start = start;
        self.bytes.resize(usize::try_from(end - start)?, 0);
        if let Err(error) = stream.read_exact(&mut self.bytes[keep..]) {
            // The cursor may have advanced partway. Force a seek on retry.
            self.bytes.clear();
            return Err(error.into());
        }
        Ok(())
    }
}

struct Scan<'a> {
    file: &'a File,
    seed: u64,
    delimiter: u8,
    width: usize,
    cancel: &'a AtomicBool,
    cache_start: u64,
    bytes: &'a [u8],
}

impl Scan<'_> {
    /// Candidate cuts use the shared input bytes and require no additional
    /// disk reads, even for wide records.
    fn guess(&self, begin: u64, end: u64) -> Option<u64> {
        let from = begin - 1;
        let lo = (from - self.cache_start) as usize;
        let hi = (end - self.cache_start) as usize;
        memchr2(b'\r', b'\n', &self.bytes[lo..hi]).map(|i| from + i as u64 + 1)
    }

    /// Read the actual file past a nominal cut when necessary: a block's end
    /// is never presented to csv-core as a synthetic EOF.
    fn chunk(&self, mut start: u64, target: u64, speculative: bool) -> Result<Chunk> {
        let mut reader = scanner::at_cached(
            self.file,
            self.delimiter,
            self.seed,
            start,
            self.cache_start,
            self.bytes,
        )?;
        let mut ends = Vec::new();
        let stop = loop {
            if reader.position() >= target {
                break Stop::Boundary;
            }
            match reader.next_record(self.cancel) {
                Ok(Some(fields)) if fields == self.width => {
                    // The maximum span is one fixed block plus the record
                    // crossing its end (subject to the scanner's record cap).
                    ends.push(u32::try_from(reader.position() - start)?);
                }
                Ok(Some(_)) if speculative && ends.is_empty() => {
                    // An embedded newline can make the first tentative row
                    // appear narrow. The coordinator validates this skipped
                    // prefix before accepting the remainder of the block.
                    start = reader.position();
                }
                Ok(Some(fields)) => {
                    break Stop::Width {
                        actual: fields,
                        expected: self.width,
                    };
                }
                Ok(None) => break Stop::Eof(reader.position()),
                Err(err) => {
                    break Stop::Error(err.context("Could not index the text file records"));
                }
            }
        };
        Ok(Chunk { start, ends, stop })
    }

    fn advance(&self, index: &mut Index, target: u64, length: u64) -> Result<()> {
        let start = index.next.position.byte();
        if !index.complete && start < target {
            self.chunk(start, target, false)?.apply(0, index, length)?;
        }
        Ok(())
    }
}

enum Stop {
    Boundary,
    Width { actual: usize, expected: usize },
    Eof(u64),
    Error(anyhow::Error),
}

struct Chunk {
    start: u64,
    // Relative record ends live for one bounded wave and are never written
    // to disk. Even the shortest records require at most one entry per byte.
    ends: Vec<u32>,
    stop: Stop,
}

impl Chunk {
    fn rows_before(&self, byte: u64) -> Option<usize> {
        if self.start == byte {
            return Some(0);
        }
        let offset = u32::try_from(byte.checked_sub(self.start)?).ok()?;
        self.ends.binary_search(&offset).ok().map(|i| i + 1)
    }

    fn apply(self, skip: usize, index: &mut Index, length: u64) -> Result<()> {
        let ends = &self.ends[skip..];
        let first_row = index.next.row;
        let next_row = first_row
            .checked_add(ends.len())
            .context("Text file row count exceeds addressable memory")?;
        // Reproduce Index::advance's exact row-or-byte checkpoint spacing.
        // Jump from anchor to anchor instead of walking every row a second time.
        let mut done = 0;
        while done < ends.len() {
            let last = index.anchors.last().unwrap();
            let until_row = INDEX_ROWS - (first_row + done - last.row);
            let probe = (done + until_row).min(ends.len());
            let until_byte = last.position.byte().saturating_add(INDEX_BYTES);
            if self.start + u64::from(ends[probe - 1]) >= until_byte {
                done += ends[done..probe]
                    .partition_point(|&offset| self.start + u64::from(offset) < until_byte)
                    + 1;
            } else if probe - done == until_row {
                done = probe;
            } else {
                break;
            }
            let mut position = index.next.position.clone();
            position.set_byte(self.start + u64::from(ends[done - 1]));
            index.anchors.push(Anchor {
                row: first_row + done,
                position,
            });
        }
        if let Some(&offset) = ends.last() {
            index.next.position.set_byte(self.start + u64::from(offset));
            index.next.row = next_row;
        }
        match self.stop {
            Stop::Eof(byte) => {
                index.next.position.set_byte(byte);
                index.complete = true;
            }
            Stop::Boundary => index.complete = index.next.position.byte() >= length,
            Stop::Width { actual, expected } => anyhow::bail!(
                "Text data record {} has {actual} fields, expected {expected}",
                next_row + 1
            ),
            Stop::Error(error) => return Err(error),
        }
        Ok(())
    }
}
