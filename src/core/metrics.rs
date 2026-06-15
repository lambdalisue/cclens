//! Per-span derived values computed over the records of a span — cost metrics
//! and the representative model. See `docs/specs/events.md`.

/// The model sentinel Claude Code writes for locally-generated assistant turns,
/// excluded when choosing a span's representative model.
const SYNTHETIC_MODEL: &str = "<synthetic>";

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

/// Wall-clock duration of a span, in seconds.
///
/// `timestamps_ms` is the record timestamps in the span (epoch milliseconds, in
/// record order — the adapter normalizes Claude Code's UTC ISO-8601 to this).
/// The duration is the last timestamp minus the first; a span with fewer than
/// two timestamped records has no measurable duration and is zero. See
/// `docs/specs/events.md`.
pub fn duration_sec(timestamps_ms: &[i64]) -> f64 {
    match (timestamps_ms.first(), timestamps_ms.last()) {
        (Some(first), Some(last)) => (last - first) as f64 / 1000.0,
        _ => 0.0,
    }
}

/// The representative model of a span: the first model that is not the
/// `<synthetic>` sentinel, in record order. `None` when the span has no
/// non-synthetic assistant record. See `docs/specs/events.md`.
pub fn representative_model<'a>(models: &[&'a str]) -> Option<&'a str> {
    models
        .iter()
        .copied()
        .find(|&model| model != SYNTHETIC_MODEL)
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

    #[test]
    fn no_records_has_no_duration() {
        assert_eq!(duration_sec(&[]), 0.0);
    }

    #[test]
    fn single_record_has_no_duration() {
        // One timestamp cannot span any time.
        assert_eq!(duration_sec(&[1_700_000_000_000]), 0.0);
    }

    #[test]
    fn duration_is_last_minus_first_in_seconds() {
        // 4000ms - 1000ms = 3.0s, regardless of intermediate records.
        assert_eq!(duration_sec(&[1000, 2500, 4000]), 3.0);
    }

    #[test]
    fn sub_second_duration_keeps_fraction() {
        assert_eq!(duration_sec(&[1000, 1500]), 0.5);
    }

    #[test]
    fn no_models_has_no_representative() {
        assert_eq!(representative_model(&[]), None);
    }

    #[test]
    fn all_synthetic_has_no_representative() {
        assert_eq!(representative_model(&["<synthetic>", "<synthetic>"]), None);
    }

    #[test]
    fn first_real_model_is_representative() {
        assert_eq!(
            representative_model(&["claude-opus-4-7", "claude-sonnet-4-6"]),
            Some("claude-opus-4-7")
        );
    }

    #[test]
    fn synthetic_is_skipped_to_the_first_real_model() {
        assert_eq!(
            representative_model(&["<synthetic>", "claude-opus-4-7"]),
            Some("claude-opus-4-7")
        );
    }
}
