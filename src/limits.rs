//! Message content rules and rate limiting.

use std::time::{Duration, Instant};

/// Maximum message length in Unicode scalar values (chars), not bytes.
pub const MAX_CHARS: usize = 280;
/// Messages a client may send back-to-back before being throttled.
pub const RATE_BURST: u32 = 5;
/// Seconds to regain one message token.
pub const RATE_REFILL_SECS: f64 = 1.0;

/// Characters never allowed in chat text: control characters (which could
/// drive the terminal), bidi marks/overrides (which could spoof text
/// direction) and the Unicode line/paragraph separators.
pub fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// Remove forbidden characters. Used on everything displayed, too.
pub fn sanitize(raw: &str) -> String {
    raw.chars().filter(|c| !is_forbidden(*c)).collect()
}

/// Clean and check a message. Returns the text to send/relay, or why not.
pub fn validate_text(raw: &str, max_chars: usize) -> Result<String, String> {
    let cleaned = sanitize(raw);
    let text = cleaned.trim();
    if text.is_empty() {
        return Err("empty message".into());
    }
    let n = text.chars().count();
    if n > max_chars {
        return Err(format!("message too long ({n}/{max_chars} chars)"));
    }
    Ok(text.to_string())
}

/// Classic token bucket: `burst` tokens, one regained every `refill_secs`.
pub struct TokenBucket {
    tokens: f64,
    burst: f64,
    refill_secs: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(burst: u32, refill_secs: f64) -> Self {
        Self {
            tokens: burst as f64,
            burst: burst as f64,
            refill_secs,
            last: Instant::now(),
        }
    }

    /// Take a token, or return how long until one is available.
    pub fn try_take(&mut self) -> Result<(), Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed / self.refill_secs).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - self.tokens) * self.refill_secs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_chars_not_bytes() {
        let emoji = "😀".repeat(MAX_CHARS);
        assert!(validate_text(&emoji, MAX_CHARS).is_ok());
        assert!(validate_text(&format!("{emoji}x"), MAX_CHARS).is_err());
    }

    #[test]
    fn strips_controls_and_bidi() {
        assert_eq!(validate_text("hi\x1b[31m\u{202E}there", 100).unwrap(), "hi[31mthere");
        assert!(validate_text("\n\t\x07", 100).is_err());
        assert_eq!(validate_text("a\u{061C}b\u{2028}c\u{2029}d", 100).unwrap(), "abcd");
    }

    #[test]
    fn bucket_allows_burst_then_throttles() {
        let mut b = TokenBucket::new(3, 10.0);
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_err());
    }
}
