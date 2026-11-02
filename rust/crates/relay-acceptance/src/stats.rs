//! Medians and percentiles for AC-70's speed report.

use serde::Serialize;

/// The spread of a sample, in its unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    /// How many values.
    pub count: usize,
    /// The median.
    pub median: f64,
    /// The 95th percentile, by the nearest rank.
    pub p95: f64,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
}

impl Summary {
    /// The summary of `values`; all zero when there are none.
    pub fn of(values: &[f64]) -> Summary {
        let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let n = sorted.len();
        if n == 0 {
            return Summary {
                count: 0,
                median: 0.0,
                p95: 0.0,
                min: 0.0,
                max: 0.0,
            };
        }
        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        let rank = ((0.95 * n as f64).ceil() as usize).clamp(1, n);
        Summary {
            count: n,
            median,
            p95: sorted[rank - 1],
            min: sorted[0],
            max: sorted[n - 1],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_median_and_the_95th_percentile_of_a_hundred_values() {
        let values: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        let summary = Summary::of(&values);
        assert_eq!(summary.count, 100);
        assert_eq!(summary.median, 50.5);
        assert_eq!(summary.p95, 95.0);
        assert_eq!((summary.min, summary.max), (1.0, 100.0));
        assert_eq!(Summary::of(&[3.0]).p95, 3.0);
        assert_eq!(Summary::of(&[]).count, 0);
    }
}
