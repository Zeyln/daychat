//! The official server, built into the client from `packaging/official-server`.

const RAW: &str = include_str!("../packaging/official-server");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Official {
    /// `host:port`
    pub addr: &'static str,
    /// Full SHA-256 certificate fingerprint (64 hex characters).
    pub fingerprint: &'static str,
}

/// The official server, if this build has one.
pub fn official() -> Option<Official> {
    let line = RAW.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let (addr, fingerprint) = line.split_once(char::is_whitespace)?;
    Some(Official { addr: addr.trim(), fingerprint: fingerprint.trim() })
}

/// True if `addr` is the official server.
pub fn is_official(addr: &str) -> bool {
    official().is_some_and(|o| o.addr.eq_ignore_ascii_case(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_server_line_is_well_formed() {
        if let Some(o) = official() {
            assert!(o.addr.contains(':'), "address needs a port: {}", o.addr);
            assert_eq!(o.fingerprint.len(), 64, "use the full fingerprint");
            assert!(o.fingerprint.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert!(is_official(o.addr));
        }
    }
}
