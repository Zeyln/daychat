//! daychat-server: standalone relay. See `daychat::server`.

use std::{io, path::PathBuf, sync::Arc};

use daychat::{
    proto,
    server::{self, Logger},
};
use tokio::{net::TcpListener, sync::watch};

const USAGE: &str = "usage: daychat-server [--bind ADDR:PORT] [--secret-file PATH]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut bind = format!("0.0.0.0:{}", proto::DEFAULT_PORT);
    let mut secret_path = PathBuf::from("daychat-server.secret");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-b" | "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            "--secret-file" => secret_path = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
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

    let log: Logger = Arc::new(|m: String| eprintln!("{m}"));
    let secret = server::load_or_create_secret(&secret_path, &log)?;
    let listener = TcpListener::bind(&bind).await?;
    eprintln!("daychat-server listening on {bind}");

    // Never signalled: the standalone server runs until killed.
    let (_stop, shutdown) = watch::channel(false);
    server::serve(listener, secret, shutdown, log).await
}
