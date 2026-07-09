use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Summary {
    pub(crate) min: u64,
    pub(crate) average: f64,
    pub(crate) median: u64,
    pub(crate) p90: u64,
    pub(crate) p95: u64,
    pub(crate) p99: u64,
    pub(crate) p999: u64,
    pub(crate) max: u64,
}

impl Summary {
    pub(crate) fn from_samples(samples: &[Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }

        let mut values: Vec<u64> = samples
            .iter()
            .map(|sample| sample.as_nanos().min(u64::MAX as u128) as u64)
            .collect();
        values.sort_unstable();
        let total: u128 = values.iter().map(|&value| value as u128).sum();

        Some(Self {
            min: values[0],
            average: total as f64 / values.len() as f64,
            median: percentile(&values, 0.5),
            p90: percentile(&values, 0.9),
            p95: percentile(&values, 0.95),
            p99: percentile(&values, 0.99),
            p999: percentile(&values, 0.999),
            max: *values.last().expect("non-empty samples"),
        })
    }
}

pub fn print_compact_summary(samples: &[Duration]) {
    let Some(summary) = Summary::from_samples(samples) else {
        println!("no data");
        return;
    };

    println!(
        "mean={:.3} ms p50={:.3} ms p90={:.3} ms p95={:.3} ms p99={:.3} ms max={:.3} ms",
        summary.average / 1_000_000.0,
        summary.median as f64 / 1_000_000.0,
        summary.p90 as f64 / 1_000_000.0,
        summary.p95 as f64 / 1_000_000.0,
        summary.p99 as f64 / 1_000_000.0,
        summary.max as f64 / 1_000_000.0,
    );
}

/// Nearest-rank percentile. This makes the tail samples visible instead of
/// interpolating them away in a relatively short benchmark.
fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

pub fn print_summary(title: &str, samples: &[Duration]) {
    println!("\n{title}");
    let Some(summary) = Summary::from_samples(samples) else {
        println!("  no samples");
        return;
    };

    println!("  minimum: {}", format_ns(summary.min as f64));
    println!("  average: {}", format_ns(summary.average));
    println!("  median:  {}", format_ns(summary.median as f64));
    println!("  p95:     {}", format_ns(summary.p95 as f64));
    println!("  p99:     {}", format_ns(summary.p99 as f64));
    println!("  p99.9:   {}", format_ns(summary.p999 as f64));
    println!("  maximum: {}", format_ns(summary.max as f64));
}

pub fn print_histogram(title: &str, samples: &[Duration], bucket_count: usize) {
    if samples.is_empty() || bucket_count == 0 {
        return;
    }

    let values: Vec<u64> = samples
        .iter()
        .map(|sample| sample.as_nanos().min(u64::MAX as u128) as u64)
        .collect();
    let max = *values.iter().max().expect("non-empty samples");
    let width = max.div_ceil(bucket_count as u64).max(1);
    let mut buckets = vec![0_usize; bucket_count];
    for value in values {
        let index = (value / width) as usize;
        buckets[index.min(bucket_count - 1)] += 1;
    }
    let peak = *buckets.iter().max().expect("non-empty buckets");

    println!("\n{title}");
    for (index, count) in buckets.into_iter().enumerate() {
        let low = index as u64 * width;
        let high = (index as u64 + 1) * width;
        let bar_len = (count * 50).checked_div(peak).unwrap_or(0);
        println!(
            "  {:>10} - {:>10} | {:<50} {}",
            format_ns(low as f64),
            format_ns(high as f64),
            "#".repeat(bar_len),
            count
        );
    }
}

pub fn format_duration(duration: Duration) -> String {
    format_ns(duration.as_nanos() as f64)
}

fn format_ns(ns: f64) -> String {
    if ns >= 1_000_000_000.0 {
        format!("{:.3} s", ns / 1_000_000_000.0)
    } else if ns >= 1_000_000.0 {
        format!("{:.3} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.3} us", ns / 1_000.0)
    } else {
        format!("{ns:.0} ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_select_observed_samples() {
        let values: Vec<u64> = (1..=1000).collect();
        assert_eq!(percentile(&values, 0.5), 500);
        assert_eq!(percentile(&values, 0.9), 900);
        assert_eq!(percentile(&values, 0.95), 950);
        assert_eq!(percentile(&values, 0.999), 999);
    }

    #[test]
    fn summary_handles_one_sample() {
        let summary = Summary::from_samples(&[Duration::from_nanos(42)]).unwrap();
        assert_eq!(summary.min, 42);
        assert_eq!(summary.p999, 42);
        assert_eq!(summary.max, 42);
    }
}
