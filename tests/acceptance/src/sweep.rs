//! Running checkpoint sweeps and independent runs on several threads, shared by the M2,
//! M3, and M5 harnesses. Only the order of work varies with the threads; every result
//! is collected by its index or its checkpoint, so the outcome does not.

use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// The threads a sweep uses: every available core.
pub fn threads() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

/// Checkpoints `range` split into at most `parts` contiguous, non-empty ranges of equal
/// length, each checkpoint costing the same.
pub fn split(range: Range<usize>, parts: usize) -> Vec<Range<usize>> {
    let parts = parts.clamp(1, range.len().max(1));
    let (len, extra) = (range.len() / parts, range.len() % parts);
    let mut start = range.start;
    (0..parts)
        .map(|i| {
            let end = start + len + usize::from(i < extra);
            let part = start..end;
            start = end;
            part
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// Runs `f` on each of `parts`, one thread per part, and returns the results in the
/// parts' order.
pub fn on_threads<T: Send>(
    parts: Vec<Range<usize>>,
    f: impl Fn(Range<usize>) -> T + Sync,
) -> Vec<T> {
    let f = &f;
    thread::scope(|s| {
        let handles: Vec<_> = parts
            .into_iter()
            .map(|part| s.spawn(move || f(part)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a checkpoint thread panicked"))
            .collect()
    })
}

/// The outcome of resuming a range of checkpoints.
#[derive(Clone, Debug, Default)]
pub struct Sweep {
    /// Checkpoints resumed.
    pub checked: usize,
    /// Every failure, as `(k, why)`.
    pub failures: Vec<(usize, String)>,
}

impl Sweep {
    /// The parts' outcomes as one, failures by checkpoint.
    pub fn merged(parts: Vec<Sweep>) -> Sweep {
        let mut sweep = Sweep::default();
        for part in parts {
            sweep.checked += part.checked;
            sweep.failures.extend(part.failures);
        }
        sweep.failures.sort_by_key(|f| f.0);
        sweep
    }
}

/// The most runs [`parallel`] keeps in flight.
const WORKERS: usize = 4;

/// Runs `jobs` on up to four threads, in any order, keeping each result at its job's
/// index.
pub fn parallel<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new(jobs.iter().map(|_| None).collect());
    thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let r = f(job);
                    out.lock().expect("unpoisoned")[i] = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .expect("unpoisoned")
        .into_iter()
        .map(|r| r.expect("every job ran"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_the_range_in_order() {
        assert_eq!(split(0..10, 3), vec![0..4, 4..7, 7..10]);
        assert_eq!(split(5..7, 4), vec![5..6, 6..7]);
        assert_eq!(split(3..3, 2), Vec::<Range<usize>>::new());
    }

    #[test]
    fn results_keep_their_order() {
        assert_eq!(on_threads(split(0..10, 3), |r| r.start), vec![0, 4, 7]);
        assert_eq!(parallel(&[3, 1, 2], |&x| x * 10), vec![30, 10, 20]);
        let merged = Sweep::merged(vec![
            Sweep {
                checked: 2,
                failures: vec![(5, "b".into())],
            },
            Sweep {
                checked: 3,
                failures: vec![(1, "a".into())],
            },
        ]);
        assert_eq!(merged.checked, 5);
        assert_eq!(merged.failures, vec![(1, "a".into()), (5, "b".into())]);
    }
}
