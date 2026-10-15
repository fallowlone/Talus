//! `talus-server serve --data DIR [--listen ADDR]` / `talus-server set-user --data DIR NAME`

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use talus_server::Store;

const USAGE: &str = "usage:
  talus-server serve --data DIR [--listen ADDR]   (default 127.0.0.1:8787; put a TLS proxy in front)
  talus-server set-user --data DIR NAME           (creates the account or resets its password;
                                                   password from $TALUS_SERVER_PASSWORD or the first stdin line)";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = PathBuf::from(flag(&args, "--data").ok_or(USAGE)?);
    match args.first().map(String::as_str) {
        Some("serve") => {
            let addr: SocketAddr = flag(&args, "--listen")
                .unwrap_or_else(|| "127.0.0.1:8787".into())
                .parse()
                .map_err(|e| format!("--listen: {e}"))?;
            let store = Arc::new(Store::open(&data)?);
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
                eprintln!("talus-server listening on http://{addr}");
                talus_server::serve(listener, store).await.map_err(|e| e.to_string())
            })
        }
        Some("set-user") => {
            let name = args.last().filter(|a| !a.starts_with("--") && args.len() > 3).ok_or(USAGE)?;
            let password = match std::env::var("TALUS_SERVER_PASSWORD") {
                Ok(p) => p,
                Err(_) => {
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
                    line.trim_end_matches(['\r', '\n']).to_string()
                }
            };
            Store::open(&data)?.set_user(name, &password).map_err(|e| e.message)?;
            eprintln!("user {name} saved");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
