//! TLS for every connection (rustls, ring provider).
//!
//! Server certificates are trusted in one of two ways:
//! - **CA-signed** (e.g. Let's Encrypt via `--cert/--key`): checked against
//!   the Mozilla root store, like a browser does.
//! - **Self-signed** (the default, also for `/host`): trust on first use.
//!   The client pins the certificate's SHA-256 fingerprint on the first
//!   visit and refuses to connect if it ever changes, like SSH's
//!   known_hosts. Hosts can share the fingerprint so joiners can check it.

use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
    sync::{Arc, Mutex},
};

use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    crypto::{ring, CryptoProvider},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Full SHA-256 fingerprint of a certificate, lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

/// Short, readable form of a fingerprint for people to compare.
pub fn short(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .take(6)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

// --- Server ----------------------------------------------------------------------

/// Create a self-signed certificate and key if neither file exists.
/// Returns whether new files were written.
pub fn create_self_signed(cert_path: &Path, key_path: &Path) -> io::Result<bool> {
    if cert_path.exists() || key_path.exists() {
        return Ok(false);
    }
    let ck = rcgen::generate_simple_self_signed(vec!["daychat".to_string()]).map_err(io::Error::other)?;
    for path in [cert_path, key_path] {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
    }
    std::fs::write(cert_path, ck.cert.pem())?;
    write_private(key_path, ck.key_pair.serialize_pem().as_bytes())?;
    Ok(true)
}

/// Write a file only its owner can read (on Unix).
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

/// Load a PEM certificate chain and key. Returns the config and the leaf
/// certificate's fingerprint.
pub fn server_config(cert_path: &Path, key_path: &Path) -> io::Result<(Arc<ServerConfig>, String)> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(File::open(cert_path)?)).collect::<Result<_, _>>()?;
    let leaf = certs.first().ok_or_else(|| io::Error::other("no certificate found in the cert file"))?;
    let fp = fingerprint(leaf.as_ref());
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(key_path)?))?
        .ok_or_else(|| io::Error::other("no private key found in the key file"))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    Ok((Arc::new(config), fp))
}

// --- Client ----------------------------------------------------------------------

/// How the server's certificate was trusted on this connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Signed by a public CA for this host name.
    Verified,
    /// Matched the fingerprint pinned on an earlier visit.
    Pinned,
    /// First visit to a self-signed server: pin this fingerprint.
    NewPin(String),
}

/// Where the verifier records how the certificate was trusted.
pub type TrustSlot = Arc<Mutex<Option<Trust>>>;

/// Which certificates the client accepts for a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// A CA-signed certificate for the host name, or else the fingerprint
    /// pinned on the first visit (`None` = first visit).
    Tofu(Option<String>),
    /// Exactly this certificate and nothing else, not even a CA-signed one.
    /// Used for the official server, whose fingerprint is built in.
    Exact(String),
}

#[derive(Debug)]
struct TofuVerifier {
    webpki: Arc<WebPkiServerVerifier>,
    pin: Pin,
    provider: Arc<CryptoProvider>,
    outcome: TrustSlot,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let set = |t: Trust| *self.outcome.lock().unwrap() = Some(t);
        let fp = fingerprint(end_entity.as_ref());
        let pinned = match &self.pin {
            Pin::Exact(expected) => {
                return if *expected == fp {
                    set(Trust::Pinned);
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(rustls::Error::General(format!(
                        "this is NOT the official daychat server (certificate {}, expected {}). \
                         Someone may be intercepting the connection.",
                        short(&fp),
                        short(expected)
                    )))
                };
            }
            Pin::Tofu(pinned) => pinned,
        };
        if self
            .webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .is_ok()
        {
            set(Trust::Verified);
            return Ok(ServerCertVerified::assertion());
        }
        match pinned {
            Some(pin) if *pin == fp => {
                set(Trust::Pinned);
                Ok(ServerCertVerified::assertion())
            }
            Some(pin) => Err(rustls::Error::General(format!(
                "SERVER CERTIFICATE CHANGED (pinned {}, now {}). Someone may be intercepting the \
                 connection. If the host really replaced it, run /unpin and join again.",
                short(pin),
                short(&fp)
            ))),
            None => {
                set(Trust::NewPin(fp));
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    // The handshake signature is always checked, so the server must hold
    // the private key of whichever certificate was accepted above.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Client config for one connection, accepting certificates per `pin`.
/// After the handshake, the returned slot says how the certificate was
/// trusted.
pub fn client_config(pin: Pin) -> io::Result<(Arc<ClientConfig>, TrustSlot)> {
    let provider = provider();
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(io::Error::other)?;
    let outcome = Arc::new(Mutex::new(None));
    let verifier = TofuVerifier { webpki, pin, provider: provider.clone(), outcome: outcome.clone() };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok((Arc::new(config), outcome))
}

/// The TLS server name for a `host:port` (or `[v6]:port`) address.
pub fn server_name(addr: &str) -> io::Result<ServerName<'static>> {
    let host = match addr.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => h,
        _ => addr,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    ServerName::try_from(host.to_string()).map_err(|_| io::Error::other(format!("invalid server name '{host}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_fingerprints() {
        assert!(matches!(server_name("127.0.0.1:7878").unwrap(), ServerName::IpAddress(_)));
        assert!(matches!(server_name("[::1]:7878").unwrap(), ServerName::IpAddress(_)));
        assert!(matches!(server_name("chat.example:7878").unwrap(), ServerName::DnsName(_)));
        let fp = fingerprint(b"x");
        assert_eq!(fp.len(), 64);
        assert_eq!(short(&fp).split(' ').count(), 6);
    }
}
