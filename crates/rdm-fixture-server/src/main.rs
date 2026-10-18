//! Standalone fixture server for manual benchmarks.
//!
//! ```text
//! rdm-fixture-server --size 32M --delay-ms 30 --port 18080 --etag '"v1"'
//! rdm-fixture-server --size 8M --no-ranges --no-length
//! rdm-fixture-server --size 4M --flaky 3 --retry-after 2
//! ```
//!
//! Prints the bound URL on stdout and serves until Ctrl-C.

use std::net::SocketAddr;
use std::process::ExitCode;

use rdm_fixture_server::{Config, TestServer};

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("rdm-fixture-server: {message}");
            eprintln!("{}", Args::usage());
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rdm-fixture-server: failed to start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
        let mut config = Config::new(args.size);
        config.delay_ms = args.delay_ms;
        config.delay_every_bytes = rdm_fixture_server::DEFAULT_DELAY_EVERY_BYTES;
        if args.no_ranges {
            config.supports_ranges = false;
        }
        if args.no_length {
            config.no_content_length = true;
        }
        if let Some(etag) = args.etag {
            config.etag = Some(etag);
        }
        if let Some(last_modified) = args.last_modified {
            config.last_modified = Some(last_modified);
        }
        if let Some(disposition) = args.disposition {
            config.content_disposition = Some(disposition);
        }
        if let Some(ordinal) = args.flaky {
            config.abort_on_requests.push(ordinal);
        }
        if let Some(first_requests) = args.retry_after {
            config.rate_limit = Some(rdm_fixture_server::RateLimitInjection {
                first_requests,
                retry_after_secs: 1,
                status: 429,
            });
        }
        if let Some(seed) = args.seed {
            config.seed = seed;
        }

        let server = match TestServer::start_at(addr, config).await {
            Ok(server) => server,
            Err(error) => {
                eprintln!("rdm-fixture-server: failed to bind {addr}: {error}");
                return ExitCode::FAILURE;
            }
        };

        println!(
            "rdm-fixture-server listening on {} (size={} ranges={} delay={}ms/{}B)",
            server.file_url(),
            args.size,
            if args.no_ranges { "ignored" } else { "bytes" },
            args.delay_ms,
            rdm_fixture_server::DEFAULT_DELAY_EVERY_BYTES,
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let _ = tokio::signal::ctrl_c().await;
        println!("rdm-fixture-server shutting down");
        server.shutdown().await;
        ExitCode::SUCCESS
    })
}

#[derive(Debug)]
struct Args {
    size: u64,
    port: u16,
    delay_ms: u64,
    no_ranges: bool,
    no_length: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    disposition: Option<String>,
    flaky: Option<u64>,
    retry_after: Option<u64>,
    seed: Option<u64>,
}

impl Args {
    fn usage() -> &'static str {
        "usage: rdm-fixture-server [--size BYTES] [--port N] [--delay-ms N] [--no-ranges]\n\
         \x20                        [--no-length] [--etag VALUE] [--last-modified VALUE]\n\
         \x20                        [--disposition VALUE] [--flaky N] [--retry-after N]\n\
         \x20                        [--seed N]"
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self {
            size: 8 * 1024 * 1024,
            port: 0,
            delay_ms: 0,
            no_ranges: false,
            no_length: false,
            etag: None,
            last_modified: None,
            disposition: None,
            flaky: None,
            retry_after: None,
            seed: None,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--size" => parsed.size = parse_bytes(&next_value(&mut args, "--size")?)?,
                "--port" => {
                    parsed.port = next_value(&mut args, "--port")?
                        .parse()
                        .map_err(parse_err)?
                }
                "--delay-ms" => {
                    parsed.delay_ms = next_value(&mut args, "--delay-ms")?
                        .parse()
                        .map_err(parse_err)?
                }
                "--no-ranges" => parsed.no_ranges = true,
                "--no-length" => parsed.no_length = true,
                "--etag" => parsed.etag = Some(next_value(&mut args, "--etag")?),
                "--last-modified" => {
                    parsed.last_modified = Some(next_value(&mut args, "--last-modified")?)
                }
                "--disposition" => {
                    parsed.disposition = Some(next_value(&mut args, "--disposition")?)
                }
                "--flaky" => {
                    parsed.flaky = Some(
                        next_value(&mut args, "--flaky")?
                            .parse()
                            .map_err(parse_err)?,
                    )
                }
                "--retry-after" => {
                    parsed.retry_after = Some(
                        next_value(&mut args, "--retry-after")?
                            .parse()
                            .map_err(parse_err)?,
                    )
                }
                "--seed" => {
                    parsed.seed = Some(
                        next_value(&mut args, "--seed")?
                            .parse()
                            .map_err(parse_err)?,
                    )
                }
                "--help" | "-h" => return Err("help requested".to_string()),
                other => return Err(format!("unknown argument `{other}`")),
            }
        }
        Ok(parsed)
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_err(error: std::num::ParseIntError) -> String {
    format!("invalid number: {error}")
}

/// Parse `123`, `4K`, `8M`, `1G` (KiB/MiB/GiB).
fn parse_bytes(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (digits, multiplier) = match value.chars().last() {
        Some('k') | Some('K') => (&value[..value.len() - 1], 1024u64),
        Some('m') | Some('M') => (&value[..value.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let base: u64 = digits.trim().parse().map_err(parse_err)?;
    Ok(base * multiplier)
}
