//! Signed self-update from GitHub Releases.
//!
//! A release is only installed if its `SHA256SUMS` carries a valid minisign
//! signature from the release key built into this binary, and the signed
//! trusted comment names that exact release (`daychat vX.Y.Z`). Control of
//! the GitHub account alone is therefore not enough to push code to users,
//! and an old signed release can't be passed off as a newer one.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

pub const REPO: &str = "Zeyln/daychat";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Set to skip update checks (development builds, tests).
pub const DISABLE_ENV: &str = "DAYCHAT_NO_UPDATE_CHECK";
const RELEASE_KEY: &str = include_str!("../packaging/release.pub");
const MAX_SMALL: u64 = 64 * 1024;
const MAX_ASSET: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    /// `(name, download url)`
    assets: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A bare client executable that replaces the running one.
    Binary,
    /// An Arch package, installed with `sudo pacman -U`.
    ArchPackage,
}

/// A downloaded, signature-checked update, ready to install.
#[derive(Debug, Clone)]
pub struct Ready {
    pub tag: String,
    pub file: PathBuf,
    pub kind: Kind,
}

pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// True if `tag` is a newer version than this binary.
pub fn is_newer(tag: &str) -> bool {
    matches!((parse_version(tag), parse_version(CURRENT)), (Some(a), Some(b)) if a > b)
}

/// TLS for talking to GitHub. Trusts the OS certificate store as well as
/// the built-in roots, so the check also works behind TLS-inspecting
/// proxies (whose CA is installed in the OS). That's safe here because
/// the connection isn't what protects updates: a proxy could block one,
/// but never forge one past the signature check.
fn tls_config() -> Option<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(Arc::new(config))
}

fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("daychat/", env!("CARGO_PKG_VERSION")));
    if let Some(tls) = tls_config() {
        builder = builder.tls_config(tls);
    }
    builder.build()
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let resp = agent().get(url).call().map_err(|e| format!("download failed: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(buf)
}

/// The latest published release (drafts and pre-releases are skipped).
/// Blocking.
pub fn latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = download(&url, 1024 * 1024)?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| "malformed release info".to_string())?;
    let tag = v["tag_name"].as_str().ok_or("malformed release info")?.to_string();
    let assets = v["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["browser_download_url"].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

fn installed_by_pacman() -> bool {
    cfg!(target_os = "linux")
        && std::env::current_exe().map(|p| p.starts_with("/usr/")).unwrap_or(false)
        && Path::new("/usr/bin/pacman").exists()
}

/// The release asset this installation should update from.
fn wanted_asset(tag: &str) -> Option<(String, Kind)> {
    if installed_by_pacman() && cfg!(target_arch = "x86_64") {
        let ver = tag.trim_start_matches('v');
        return Some((format!("daychat-{ver}-1-x86_64.pkg.tar.zst"), Kind::ArchPackage));
    }
    let target = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        return None;
    };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    Some((format!("daychat-client-{target}{ext}"), Kind::Binary))
}

/// Check `SHA256SUMS` against its signature and the release it claims to be.
fn verify_sums(sums: &[u8], sig: &str, tag: &str) -> Result<(), String> {
    let key_line = RELEASE_KEY
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .ok_or("no release key is built into this daychat")?;
    let key = PublicKey::from_base64(key_line).map_err(|_| "the built-in release key is invalid")?;
    let sig = Signature::decode(sig).map_err(|_| "the release signature file is malformed")?;
    key.verify(sums, &sig, false)
        .map_err(|_| "the release signature is INVALID; refusing to update")?;
    if sig.trusted_comment() != format!("daychat {tag}") {
        return Err("the signature belongs to a different release; refusing to update".into());
    }
    Ok(())
}

/// The expected hash of `name` in a `sha256sum`-format file.
fn sums_entry(sums: &[u8], name: &str) -> Option<String> {
    String::from_utf8_lossy(sums).lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Download this platform's asset and verify it. Blocking.
pub fn fetch(release: &Release) -> Result<Ready, String> {
    let (name, kind) =
        wanted_asset(&release.tag).ok_or("there is no prebuilt daychat for this platform; update from source")?;
    let url = |n: &str| {
        release
            .assets
            .iter()
            .find(|(a, _)| a == n)
            .map(|(_, u)| u.clone())
            .ok_or(format!("release {} has no {n}", release.tag))
    };

    let sums = download(&url("SHA256SUMS")?, MAX_SMALL)?;
    let sig = download(&url("SHA256SUMS.minisig")?, MAX_SMALL)?;
    verify_sums(&sums, &String::from_utf8_lossy(&sig), &release.tag)?;
    let expected = sums_entry(&sums, &name).ok_or(format!("{name} is not in the signed checksums"))?;

    let data = download(&url(&name)?, MAX_ASSET)?;
    let actual: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        return Err(format!("{name} does not match the signed checksum; download rejected"));
    }

    let dir = std::env::temp_dir().join(format!("daychat-update-{}", release.tag));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(&name);
    std::fs::write(&file, &data).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(Ready { tag: release.tag.clone(), file, kind })
}

/// Install a verified update. Call with the terminal restored: on Arch
/// this runs `sudo pacman -U`, which asks for a password.
pub fn install(ready: &Ready) -> Result<(), String> {
    match ready.kind {
        Kind::Binary => self_replace::self_replace(&ready.file).map_err(|e| format!("could not replace daychat: {e}")),
        Kind::ArchPackage => {
            let status = Command::new("sudo")
                .arg("pacman")
                .arg("-U")
                .arg(&ready.file)
                .status()
                .map_err(|e| format!("could not run sudo pacman: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("pacman did not install the update".into())
            }
        }
    }
}

/// Run `exe` (the updated daychat) with this process's arguments in its
/// place. Pass the path captured *before* installing: once the file is
/// replaced, `current_exe` may no longer resolve to it.
pub fn restart(exe: &Path) -> io::Error {
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(0)),
            Err(e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v1.2"), None);
        assert!(is_newer("v999.0.0"));
        assert!(!is_newer(&format!("v{CURRENT}")));
        assert!(!is_newer("v0.0.1"));
        assert!(!is_newer("garbage"));
    }

    #[test]
    fn checksum_lines() {
        let sums = b"ABCDEF  daychat-client-x\n0123 *other\n";
        assert_eq!(sums_entry(sums, "daychat-client-x").as_deref(), Some("abcdef"));
        assert_eq!(sums_entry(sums, "other").as_deref(), Some("0123"));
        assert_eq!(sums_entry(sums, "missing"), None);
    }

    #[test]
    fn built_in_key_is_valid_and_rejects_forgeries() {
        let line = RELEASE_KEY.lines().find(|l| !l.starts_with("untrusted comment:")).unwrap();
        assert!(PublicKey::from_base64(line.trim()).is_ok());
        // Garbage and well-formed-but-wrong signatures are refused.
        assert!(verify_sums(b"x", "not a signature", "v1.0.0").is_err());
    }
}
