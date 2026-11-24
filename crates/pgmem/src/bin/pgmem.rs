//! `pgmem` — run a standalone pgmem server (for psql, JDBC, pgx and other clients).
//!
//! ```text
//! pgmem [--host 127.0.0.1] [--port 5432] [--now '2024-07-15 12:00:00+00']
//! ```
//!
//! Prints the connection string on stdout and serves until interrupted. `--port 0` picks a free
//! port; `--now` pins `now()` (Postgres timestamptz syntax), like `Server::set_now`.

fn usage() -> ! {
    eprintln!("usage: pgmem [--host HOST] [--port PORT] [--now TIMESTAMPTZ]");
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
            "--now" => {
                let Some(t) = args.next().and_then(|v| pgmem_types::datetime::parse_timestamptz(&v).ok()) else { usage() };
                let unix_micros = t + pgmem_types::datetime::PG_EPOCH_UNIX_SECS * 1_000_000;
                let Ok(micros) = u64::try_from(unix_micros) else { usage() };
                builder = builder.now(std::time::UNIX_EPOCH + std::time::Duration::from_micros(micros));
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
    // Flush so a parent process reading the DSN line sees it immediately.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    loop {
        std::thread::park();
    }
}
