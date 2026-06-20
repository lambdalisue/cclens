//! Behavioral classification of user prompts. The point is not what a prompt is
//! about (topic — which embeddings cluster, but that does not map to reusable
//! skills) but how the user is *steering*: approving, correcting, asking, or
//! directing. The mix is an actionable signal — heavy steering suggests room for
//! more autonomy, frequent corrections suggest clearer upfront specs. These are
//! lexical heuristics, so reports label them lower-confidence.

/// How a user prompt steers the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptBehavior {
    /// A bare approval / go-ahead that carries no task ("go ahead", "yes", "次").
    Steer,
    /// A correction or redirect after a wrong turn ("いや…戻して", "no, instead").
    Correct,
    /// A question rather than a directive (ends with `?` / `？`).
    Question,
    /// A substantive instruction — the default.
    Instruct,
}

impl PromptBehavior {
    pub fn label(self) -> &'static str {
        match self {
            PromptBehavior::Steer => "steer",
            PromptBehavior::Correct => "correct",
            PromptBehavior::Question => "question",
            PromptBehavior::Instruct => "instruct",
        }
    }
}

/// Classify a prompt by how it steers the session. Order matters: a bare
/// approval is checked before correction/question so "yes" is steering, not a
/// (missing) directive.
pub fn classify_prompt(text: &str) -> PromptBehavior {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    if is_steer(trimmed, &lower) {
        PromptBehavior::Steer
    } else if is_correction(trimmed, &lower) {
        PromptBehavior::Correct
    } else if trimmed.ends_with('?') || trimmed.ends_with('？') {
        PromptBehavior::Question
    } else {
        PromptBehavior::Instruct
    }
}

/// A whole-prompt approval that carries no task of its own.
fn is_steer(trimmed: &str, lower: &str) -> bool {
    const APPROVALS: &[&str] = &[
        "go ahead",
        "go",
        "yes",
        "yes please",
        "y",
        "ok",
        "okay",
        "sure",
        "next",
        "continue",
        "proceed",
        "続けて",
        "続け",
        "進めて",
        "完遂して",
        "完遂してね",
        "どうぞ",
        "うん",
        "はい",
        "これで",
        "お願い",
        "おねがい",
        "やって",
        "それで",
    ];
    if APPROVALS.contains(&lower) {
        return true;
    }
    // A very short prompt that is not a question is a steering token (a menu
    // pick like "A"/"2", "go ahead.", a one-word nudge).
    let chars = trimmed.chars().count();
    chars > 0 && chars <= 4 && !trimmed.ends_with('?') && !trimmed.ends_with('？')
}

/// A correction or redirect: the previous turn was wrong and is being undone or
/// re-pointed.
fn is_correction(trimmed: &str, lower: &str) -> bool {
    const LEADING_JP: &[&str] = &["いや", "ちが", "違", "そうじゃ", "じゃなくて"];
    const ANYWHERE_JP: &[&str] = &[
        "じゃなくて",
        "やり直",
        "戻して",
        "間違",
        "そんなこと",
        "違うよ",
        "ではない",
    ];
    const LEADING_EN: &[&str] = &["no,", "no ", "not ", "actually,", "wait,"];
    const ANYWHERE_EN: &[&str] = &["instead", "revert", "undo", "that's wrong", "rollback"];

    if LEADING_JP.iter().any(|m| trimmed.starts_with(m)) {
        return true;
    }
    if ANYWHERE_JP.iter().any(|m| trimmed.contains(m)) {
        return true;
    }
    if LEADING_EN.iter().any(|m| lower.starts_with(m)) {
        return true;
    }
    ANYWHERE_EN.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_approvals_are_steering() {
        assert_eq!(classify_prompt("go ahead"), PromptBehavior::Steer);
        assert_eq!(classify_prompt("yes"), PromptBehavior::Steer);
        assert_eq!(classify_prompt("次"), PromptBehavior::Steer);
        assert_eq!(classify_prompt("B"), PromptBehavior::Steer); // a menu pick
        assert_eq!(classify_prompt("完遂してね"), PromptBehavior::Steer);
    }

    #[test]
    fn corrections_and_redirects_are_correct() {
        assert_eq!(
            classify_prompt("いや、それは違う。戻して"),
            PromptBehavior::Correct
        );
        assert_eq!(classify_prompt("そうじゃなくて逆"), PromptBehavior::Correct);
        assert_eq!(
            classify_prompt("no, do it the other way instead"),
            PromptBehavior::Correct
        );
    }

    #[test]
    fn trailing_question_mark_is_a_question() {
        assert_eq!(
            classify_prompt("なぜそうなるの？"),
            PromptBehavior::Question
        );
        assert_eq!(classify_prompt("is that a bug?"), PromptBehavior::Question);
    }

    #[test]
    fn a_substantive_directive_is_an_instruction() {
        assert_eq!(
            classify_prompt("次の契約を TDD で実装して、テストも書いて"),
            PromptBehavior::Instruct
        );
    }

    #[test]
    fn a_correction_wins_over_a_nudge() {
        // Leading "いや" wins even though it could read as a nudge.
        assert_eq!(
            classify_prompt("いや、それは保留で"),
            PromptBehavior::Correct
        );
    }
}
