//! daychat-server: standalone relay. See `daychat::server`.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use daychat::{
    proto,
    server::{self, Logger},
    tls,
};
use tokio::{net::TcpListener, sync::watch};

const USAGE: &str = "usage: daychat-server [--bind ADDR:PORT] [--secret-file PATH] [--cert PEM --key PEM]

  --bind ADDR:PORT    where to listen (default 0.0.0.0:7878)
  --secret-file PATH  server secret; keeps IDs stable across restarts
                      (default ./daychat-server.secret, created if missing)
  --cert, --key       TLS certificate chain and private key (PEM), e.g. from
                      Let's Encrypt. Without them, a self-signed certificate
                      is created next to the secret file and clients pin it
                      on first use.";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

/// `name` in the same directory as `secret`.
fn beside(secret: &Path, name: &str) -> PathBuf {
    secret.parent().map(|d| d.join(name)).unwrap_or_else(|| PathBuf::from(name))
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut bind = format!("0.0.0.0:{}", proto::DEFAULT_PORT);
    let mut secret_path = PathBuf::from("daychat-server.secret");
    let (mut cert, mut key) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-b" | "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            "--secret-file" => secret_path = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--cert" => cert = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "--key" => key = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage()
            }
        }
    }

    // Check arguments before creating any files.
    if cert.is_some() != key.is_some() {
        eprintln!("--cert and --key must be given together");
        usage()
    }

    let log: Logger = Arc::new(|m: String| eprintln!("{m}"));
    let secret = server::load_or_create_secret(&secret_path, &log)?;

    let (cert, key) = match (cert, key) {
        (Some(c), Some(k)) => (c, k),
        (None, None) => {
            let (c, k) = (beside(&secret_path, "daychat-server.crt"), beside(&secret_path, "daychat-server.key"));
            if tls::create_self_signed(&c, &k)? {
                eprintln!("created self-signed TLS certificate at {}", c.display());
            }
            (c, k)
        }
        _ => unreachable!("checked above"),
    };
    let (config, fingerprint) = tls::server_config(&cert, &key)?;

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("daychat-server listening on {bind} (TLS)");
    eprintln!("certificate fingerprint: {}", tls::short(&fingerprint));
    eprintln!("  full: {fingerprint}");
    eprintln!("  share it so people can check it the first time they join");

    // Never signalled: the standalone server runs until killed.
    let (_stop, shutdown) = watch::channel(false);
    server::serve(listener, secret, config, shutdown, log).await
}
