//! Exact, bounded-memory order statistics over sortable Arrow native keys.
use anyhow::{Context, Result, ensure};
use std::cmp::Ordering;

const PROBABILITIES: [u128; 5] = [25, 50, 75, 90, 95];
const BUFFERED_VALUES: usize = 16 * 1024;
type Key = [u8; 32];

struct Rank {
    remaining: usize,
    prefix: Key,
    population: usize,
}

/// The scanner visits the same finite values on each pass, with their
/// big-endian, order-preserving keys in the first `width` bytes. The result
/// contains the exact adjacent order statistics and interpolation weights
/// (in hundredths) for P25, P50, P75, P90 and P95.
/// The comparator must have the same order as the encoded keys.
pub(crate) fn percentile_ranks<T: Copy>(
    finite: usize,
    width: usize,
    compare: impl Fn(&T, &T) -> Ordering,
    scan: impl FnMut(&mut dyn FnMut(Key, T)) -> Result<()>,
) -> Result<Option<[(T, T, usize); 5]>> {
    percentile_ranks_with_capacity(finite, width, BUFFERED_VALUES, compare, scan)
}

fn percentile_ranks_with_capacity<T: Copy>(
    finite: usize,
    width: usize,
    capacity: usize,
    compare: impl Fn(&T, &T) -> Ordering,
    mut scan: impl FnMut(&mut dyn FnMut(Key, T)) -> Result<()>,
) -> Result<Option<[(T, T, usize); 5]>> {
    if finite == 0 {
        return Ok(None);
    }
    ensure!((1..=32).contains(&width), "Invalid numeric rank key width");
    let positions = PROBABILITIES.map(|percent| (finite - 1) as u128 * percent);
    let mut targets = positions.map(|position| {
        let lower = (position / 100) as usize;
        [lower, lower + usize::from(position % 100 != 0)].map(|remaining| Rank {
            remaining,
            prefix: [0; 32],
            population: finite,
        })
    });
    let mut population = finite;
    let mut depth = 0;
    while population > capacity && depth < width {
        // Order statistics are ordered by rank, so each active prefix appears
        // consecutively. Up to ten prefix histograms coexist at a given depth.
        let mut groups = Vec::<(Key, [usize; 256])>::with_capacity(2 * targets.len());
        for target in targets.iter().flatten() {
            if groups.last().is_none_or(|(key, _)| *key != target.prefix) {
                groups.push((target.prefix, [0; 256]));
            }
        }
        scan(&mut |key, _| {
            if let Ok(group) =
                groups.binary_search_by(|(prefix, _)| prefix[..depth].cmp(&key[..depth]))
            {
                groups[group].1[key[depth] as usize] += 1;
            }
        })?;
        population = 0;
        let mut previous = None;
        for target in targets.iter_mut().flatten() {
            let group = groups
                .binary_search_by_key(&target.prefix, |(key, _)| *key)
                .expect("histogram for every target prefix");
            let mut matched = false;
            for (digit, &count) in groups[group].1.iter().enumerate() {
                if target.remaining < count {
                    target.prefix[depth] = digit as u8;
                    target.population = count;
                    if previous != Some(target.prefix) {
                        population += count;
                        previous = Some(target.prefix);
                    }
                    matched = true;
                    break;
                }
                target.remaining -= count;
            }
            ensure!(matched, "Numeric rank exceeds the available values");
        }
        depth += 1;
    }

    // Once all relevant prefixes hold at most the bounded buffer size, finish
    // in one pass. For a complete key, all values in that prefix are equal.
    let mut candidates: Vec<(Key, Vec<T>)> = Vec::with_capacity(2 * targets.len());
    for target in targets.iter().flatten() {
        if candidates
            .last()
            .is_none_or(|(key, _)| *key != target.prefix)
        {
            let capacity = if depth == width { 1 } else { target.population };
            candidates.push((target.prefix, Vec::with_capacity(capacity)));
        }
    }
    scan(&mut |key, value| {
        if let Ok(group) =
            candidates.binary_search_by(|(prefix, _)| prefix[..depth].cmp(&key[..depth]))
            && (depth < width || candidates[group].1.is_empty())
        {
            candidates[group].1.push(value);
        }
    })?;
    if depth < width {
        for (prefix, values) in &mut candidates {
            let mut end = values.len();
            // Split from the highest requested rank down. Each partition
            // leaves all previously selected ranks in place.
            for rank in targets
                .iter()
                .flatten()
                .rev()
                .filter(|rank| rank.prefix == *prefix)
            {
                if rank.remaining < end {
                    values[..end].select_nth_unstable_by(rank.remaining, &compare);
                    end = rank.remaining;
                }
            }
        }
    }
    let value_at = |rank: &Rank| -> Result<T> {
        let group = candidates
            .binary_search_by_key(&rank.prefix, |(key, _)| *key)
            .expect("buffer for every target prefix");
        let rank = if depth == width { 0 } else { rank.remaining };
        candidates[group]
            .1
            .get(rank)
            .copied()
            .context("Numeric rank is missing")
    };
    let mut result = Vec::with_capacity(targets.len());
    for (ranks, position) in targets.iter().zip(positions) {
        result.push((
            value_at(&ranks[0])?,
            value_at(&ranks[1])?,
            (position % 100) as usize,
        ));
    }
    Ok(Some(result.try_into().ok().expect("five percentile ranks")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_and_buffered_paths_select_the_same_exact_ranks() -> Result<()> {
        let values: Vec<_> = (0..10007_i64)
            .map(|index| (index * 5419) % 2183 - 1134)
            .chain([i64::MIN, 0, 0, i64::MAX])
            .collect();
        let scan = |visit: &mut dyn FnMut(Key, i64)| {
            for &value in &values {
                let mut key = [0; 32];
                key[..8].copy_from_slice(&((value as u64) ^ (1 << 63)).to_be_bytes());
                visit(key, value);
            }
            Ok(())
        };
        let mut sorted = values.clone();
        sorted.sort_unstable();
        let expected = Some(PROBABILITIES.map(|percent| {
            let rank = (values.len() - 1) * percent as usize;
            (sorted[rank / 100], sorted[rank.div_ceil(100)], rank % 100)
        }));
        assert_eq!(percentile_ranks(values.len(), 8, i64::cmp, scan)?, expected);
        for capacity in [0, 256] {
            assert_eq!(
                percentile_ranks_with_capacity(values.len(), 8, capacity, i64::cmp, scan)?,
                expected
            );
        }
        Ok(())
    }
}
