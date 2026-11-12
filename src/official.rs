//! The official server: the only server the daychat client connects to.
//!
//! Its address and full certificate fingerprint are built in from
//! `packaging/official-server`, and only that exact certificate is
//! accepted. Third-party servers are deliberately unsupported: a modified
//! server could phish users or sit in the middle of conversations whose
//! safety codes nobody compared, and most users can't judge that risk.
//!
//! **Debug builds only** may point elsewhere for development and testing:
//! `DAYCHAT_DEV_SERVER="host:port fingerprint"`. Release builds compile
//! that check out, so no user can be talked into using it.

const RAW: &str = include_str!("../packaging/official-server");
pub const DEV_ENV: &str = "DAYCHAT_DEV_SERVER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Official {
    /// `host:port`
    pub addr: String,
    /// Full SHA-256 certificate fingerprint (64 lowercase hex characters).
    pub fingerprint: String,
    /// Set from `DAYCHAT_DEV_SERVER` in a debug build.
    pub dev: bool,
}

fn parse(line: &str) -> Option<(String, String)> {
    let (addr, fp) = line.trim().split_once(char::is_whitespace)?;
    let fp = fp.trim().to_ascii_lowercase();
    let ok = addr.contains(':') && fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| (addr.to_string(), fp))
}

/// The built-in official server, if this build has one.
pub fn built_in() -> Option<Official> {
    let line = RAW.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let (addr, fingerprint) = parse(line)?;
    Some(Official { addr, fingerprint, dev: false })
}

/// The server this client connects to: the built-in official server, or in
/// debug builds a `DAYCHAT_DEV_SERVER` override.
pub fn server() -> Option<Official> {
    if cfg!(debug_assertions) {
        if let Some((addr, fingerprint)) = std::env::var(DEV_ENV).ok().as_deref().and_then(parse) {
            return Some(Official { addr, fingerprint, dev: true });
        }
    }
    built_in()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_server_line_is_well_formed() {
        // A server line that fails to parse would silently ship a client
        // that can't connect anywhere.
        let has_line = RAW.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with('#'));
        assert_eq!(has_line, built_in().is_some(), "packaging/official-server has a malformed server line");
    }

    #[test]
    fn parsing() {
        let fp = "ab".repeat(32);
        assert_eq!(parse(&format!("chat.example:7878 {fp}")), Some(("chat.example:7878".into(), fp.clone())));
        assert_eq!(parse(&format!("chat.example {fp}")), None, "needs a port");
        assert_eq!(parse("chat.example:7878 abcd"), None, "needs the full fingerprint");
    }
}
