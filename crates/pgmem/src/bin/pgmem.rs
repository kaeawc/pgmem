//! `pgmem` — run a standalone pgmem server (for psql, JDBC, pgx and other clients).
//!
//! ```text
//! pgmem [--host 127.0.0.1] [--port 5432]
//! ```
//!
//! Prints the connection string on stdout and serves until interrupted.

fn usage() -> ! {
    eprintln!("usage: pgmem [--host HOST] [--port PORT]");
    std::process::exit(2);
}

fn main() {
    let mut builder = pgmem::Server::builder();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                let Some(p) = args.next().and_then(|v| v.parse().ok()) else { usage() };
                builder = builder.port(p);
            }
            "--host" | "-h" => {
                let Some(h) = args.next() else { usage() };
                builder = builder.host(h);
            }
            "--help" => usage(),
            _ => usage(),
        }
    }
    let server = match builder.start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pgmem: {e}");
            std::process::exit(1);
        }
    };
    println!("{}", server.dsn());
    loop {
        std::thread::park();
    }
}
