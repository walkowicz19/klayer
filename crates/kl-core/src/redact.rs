//! Pattern-based PII redaction (v1.6.5 patch, Stage 2: "PII Redaction Before
//! Storage"). Deliberately regex-only, not a NER model — see patch-1.6.5.md
//! section A.1 for the explicit scoping. Each match is replaced with a typed
//! `[REDACTED:<KIND>]` placeholder so downstream readers can still tell what
//! kind of thing used to be there without seeing the raw value.
//!
//! Match order matters: email, then card, then phone, then id — each pass
//! only sees what the previous pass left behind, so a 16-digit card number
//! can never be re-classified as a phone number or a national ID once it has
//! already been redacted.

use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap())
}

/// 13-19 digit runs, optionally grouped by single spaces or dashes. No Luhn
/// check — per the doc, simple shape-matching is enough for this stage.
fn card_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").unwrap())
}

/// A leading `+`/`(` or a mandatory separator between the first two digit
/// groups is required, so a bare unformatted digit run (e.g. a national ID)
/// falls through to `id_re` instead of being swallowed here.
fn phone_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\+?\(?\d{1,4}\)?[ .\-]\d{2,4}(?:[ .\-]?\d{2,4}){0,2}").unwrap())
}

/// Generic 9-11 consecutive digits (national-ID-shaped) left over after
/// email/card/phone passes have already claimed anything more specific.
fn id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b\d{9,11}\b").unwrap())
}

/// Redact PII from `text`, returning the redacted text. Returns the original
/// string unchanged (no allocation beyond the final owned copy) when nothing
/// matches any pattern.
pub fn redact(text: &str) -> String {
    redact_report(text).0
}

/// Same as [`redact`] but also reports whether anything was redacted, for
/// logging/telemetry — never used to block a write.
pub fn redact_report(text: &str) -> (String, bool) {
    let mut redacted = false;
    let mut out: Cow<'_, str> = Cow::Borrowed(text);

    if email_re().is_match(&out) {
        redacted = true;
        out = Cow::Owned(
            email_re()
                .replace_all(&out, "[REDACTED:EMAIL]")
                .into_owned(),
        );
    }
    if card_re().is_match(&out) {
        redacted = true;
        out = Cow::Owned(card_re().replace_all(&out, "[REDACTED:CARD]").into_owned());
    }
    if phone_re().is_match(&out) {
        redacted = true;
        out = Cow::Owned(
            phone_re()
                .replace_all(&out, "[REDACTED:PHONE]")
                .into_owned(),
        );
    }
    if id_re().is_match(&out) {
        redacted = true;
        out = Cow::Owned(
            id_re()
                .replace_all(&out, "[REDACTED:ID_NUMBER]")
                .into_owned(),
        );
    }

    (out.into_owned(), redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email() {
        let (out, hit) = redact_report("contact me at test@example.com please");
        assert!(hit);
        assert_eq!(out, "contact me at [REDACTED:EMAIL] please");
    }

    #[test]
    fn redacts_credit_card_grouped() {
        let (out, hit) = redact_report("card is 4111-1111-1111-1111 exp 12/30");
        assert!(hit);
        assert!(out.contains("[REDACTED:CARD]"));
        assert!(!out.contains("4111"));
    }

    #[test]
    fn redacts_credit_card_plain() {
        let (out, hit) = redact_report("card 4111111111111111 on file");
        assert!(hit);
        assert!(out.contains("[REDACTED:CARD]"));
    }

    #[test]
    fn redacts_phone_number() {
        let (out, hit) = redact_report("call +1-555-123-4567 tomorrow");
        assert!(hit);
        assert!(out.contains("[REDACTED:PHONE]"));
        assert!(!out.contains("555"));
    }

    #[test]
    fn redacts_phone_number_with_parens() {
        let (out, hit) = redact_report("office: (555) 123-4567 front desk");
        assert!(hit);
        assert!(out.contains("[REDACTED:PHONE]"));
    }

    #[test]
    fn redacts_national_id_like_digit_run() {
        let (out, hit) = redact_report("national id 123456789 on file");
        assert!(hit);
        assert!(out.contains("[REDACTED:ID_NUMBER]"));
        assert!(!out.contains("123456789"));
    }

    #[test]
    fn clean_text_passes_through_unchanged() {
        let text = "This is a normal sentence with no PII in it at all.";
        let (out, hit) = redact_report(text);
        assert!(!hit);
        assert_eq!(out, text);
    }

    #[test]
    fn redact_wrapper_returns_same_text_as_report() {
        let text = "email me: someone@domain.org";
        assert_eq!(redact(text), redact_report(text).0);
    }

    #[test]
    fn does_not_flag_short_numbers_as_id() {
        let (out, hit) = redact_report("we have 42 widgets and order #12345");
        assert!(!hit);
        assert_eq!(out, "we have 42 widgets and order #12345");
    }
}
