//! Cost metrics computed over the records of a span. See `docs/specs/events.md`.

/// Compaction-safe context consumption for a span.
///
/// `prompt_sizes` is the prompt size (`input + cache_read + cache_creation`) at
/// each `assistant` record in the span, in order. The result is the sum of the
/// *positive* differences between consecutive sizes; decreases — from a
/// mid-span context compaction or cache eviction — clip to zero. This counts
/// what the span actually added to the running context and stays correct even
/// when prompt size is non-monotonic. See `docs/specs/events.md`.
pub fn ctx_growth(prompt_sizes: &[u64]) -> u64 {
    prompt_sizes
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_records_grows_nothing() {
        assert_eq!(ctx_growth(&[]), 0);
    }

    #[test]
    fn single_record_grows_nothing() {
        // With one point there is no delta to accumulate.
        assert_eq!(ctx_growth(&[1234]), 0);
    }

    #[test]
    fn monotonic_increase_sums_each_step() {
        // 150-100 + 220-150 = 50 + 70 = 120
        assert_eq!(ctx_growth(&[100, 150, 220]), 120);
    }

    #[test]
    fn compaction_drop_clips_to_zero() {
        // A mid-span compaction (999 -> 50) must not subtract.
        // 999-100 + max(0, 50-999) + 80-50 = 899 + 0 + 30 = 929
        assert_eq!(ctx_growth(&[100, 999, 50, 80]), 929);
    }

    #[test]
    fn all_decreasing_grows_nothing() {
        assert_eq!(ctx_growth(&[500, 400, 300]), 0);
    }
}
