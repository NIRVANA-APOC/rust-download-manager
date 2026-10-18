//! `rdm bench` — 1 vs N connection benchmark against the embedded fixture.
//!
//! The fixture injects `delay_ms` of latency per `delay_every` bytes, which
//! simulates a per-connection bandwidth cap. The benchmark reports the median
//! of `--runs` runs per connection level, throughput in MiB/s, the speedup
//! versus the first level and whether every downloaded payload hashed the same
//! (a correctness cross-check).

use std::time::Instant;

use rdm_core::DownloadRequest;
use rdm_fixture_server::{Config, TestServer};
use serde::Serialize;
use serde_json::json;

use crate::cli::BenchArgs;
use crate::output::{invalid_request, OutputMode, Reporter};
use crate::util;

#[derive(Debug, Clone, Serialize)]
struct LevelResult {
    connections: usize,
    runs_ms: Vec<u64>,
    median_ms: u64,
    min_ms: u64,
    max_ms: u64,
    mbps: f64,
    speedup_vs_base: f64,
    sha256: Option<String>,
}

pub async fn run(mode: OutputMode, args: BenchArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "bench");

    let size = match util::parse_bytes(&args.size) {
        Ok(0) => {
            return reporter.finish_err(invalid_request("--size must be greater than zero"));
        }
        Ok(size) => size,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    let delay_every = match util::parse_bytes(&args.delay_every) {
        Ok(0) => {
            return reporter.finish_err(invalid_request("--delay-every must be greater than zero"));
        }
        Ok(value) => value,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    let connection_levels = match parse_connection_levels(&args.connections) {
        Ok(levels) => levels,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    if args.runs == 0 || args.runs > 20 {
        return reporter.finish_err(invalid_request("--runs must be between 1 and 20"));
    }

    let server = match TestServer::start(
        Config::new(size)
            .with_delay(args.delay_ms)
            .with_delay_every(delay_every),
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            return reporter.finish_err(
                rdm_agent::AgentErrorInfo::new(
                    rdm_agent::ErrorCode::Internal,
                    format!("cannot start fixture server: {error}"),
                )
                .with_hint("retry; if it persists, check local port availability"),
            )
        }
    };

    let mut levels: Vec<LevelResult> = Vec::new();
    let mut reference_sha: Option<String> = None;
    let mut hash_consistent = true;
    let mut failure: Option<String> = None;

    'levels: for &connections in &connection_levels {
        let mut runs_ms = Vec::with_capacity(args.runs);
        let mut sha256 = None;
        for run in 1..=args.runs {
            let dir = match util::create_temp_dir(&format!("bench-{connections}-{run}")) {
                Ok(dir) => dir,
                Err(error) => {
                    failure = Some(format!("cannot create temp dir: {error}"));
                    break 'levels;
                }
            };
            let mut request = DownloadRequest::new(server.file_url());
            request.output_dir = Some(dir.clone());
            request.file_name = Some(format!("bench-{connections}-{run}.bin"));
            request.connections = connections;
            let started = Instant::now();
            let outcome = rdm_core::download(request, None).await;
            let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            match outcome {
                Ok(summary) => {
                    runs_ms.push(elapsed_ms);
                    match util::sha256_file(&summary.path) {
                        Ok(hash) => {
                            if let Some(reference) = &reference_sha {
                                if reference != &hash {
                                    hash_consistent = false;
                                    failure = Some(format!(
                                        "{connections}-connection payload hash differs from the reference"
                                    ));
                                }
                            } else {
                                reference_sha = Some(hash.clone());
                            }
                            sha256 = Some(hash);
                        }
                        Err(error) => {
                            failure = Some(format!("cannot hash benchmark payload: {error}"));
                        }
                    }
                }
                Err(error) => {
                    failure = Some(format!("{connections}-connection download failed: {error}"));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            if failure.is_some() {
                break 'levels;
            }
        }
        if runs_ms.is_empty() {
            break;
        }
        let mut sorted = runs_ms.clone();
        sorted.sort_unstable();
        let median_ms = median(&sorted);
        let seconds = (median_ms as f64 / 1000.0).max(1e-6);
        let mbps = (size as f64 / (1024.0 * 1024.0)) / seconds;
        levels.push(LevelResult {
            connections,
            runs_ms,
            median_ms,
            min_ms: *sorted.first().unwrap_or(&0),
            max_ms: *sorted.last().unwrap_or(&0),
            mbps,
            speedup_vs_base: 1.0,
            sha256,
        });
    }

    server.shutdown().await;

    if let Some(message) = failure {
        let error = rdm_agent::AgentErrorInfo::new(rdm_agent::ErrorCode::Internal, message)
            .with_hint("re-run with a smaller --size or `--runs 1` to isolate the failure");
        return reporter.finish_report(
            json!({
                "size_bytes": size,
                "levels": levels,
                "hash_consistent": hash_consistent,
            }),
            Some(error),
        );
    }

    // Speedup relative to the first (lowest) connection level.
    if let Some(base) = levels.first().map(|level| level.median_ms) {
        for level in &mut levels {
            if level.median_ms > 0 {
                level.speedup_vs_base = base as f64 / level.median_ms as f64;
            }
        }
    }
    let best = levels
        .iter()
        .map(|level| level.speedup_vs_base)
        .fold(1.0f64, f64::max);
    let data = json!({
        "size_bytes": size,
        "size_human": util::format_bytes(size),
        "fixture": {
            "transport": "loopback-http1",
            "delay_ms": args.delay_ms,
            "delay_every_bytes": delay_every,
            "runs": args.runs,
        },
        "levels": levels,
        "best_speedup_vs_base": best,
        "hash_consistent": hash_consistent,
        "rdm_version": env!("CARGO_PKG_VERSION"),
    });

    if reporter.is_text() {
        if let Some(levels) = data["levels"].as_array() {
            crate::outln!(
                "size {}, {} run(s) per level, fixture delay {} ms / {}{}",
                util::format_bytes(size),
                args.runs,
                args.delay_ms,
                util::format_bytes(delay_every),
                " (simulated per-connection throttle)",
            );
            crate::outln!(
                "{:>12}  {:>10}  {:>10}  {:>12}  {:>10}",
                "connections",
                "median",
                "min..max",
                "throughput",
                "speedup"
            );
            for level in levels {
                crate::outln!(
                    "{:>12}  {:>9}ms  {:>4}..{:<4}  {:>8.2} MiB/s  {:>9.2}x",
                    level["connections"],
                    level["median_ms"].as_u64().unwrap_or(0),
                    level["min_ms"].as_u64().unwrap_or(0),
                    level["max_ms"].as_u64().unwrap_or(0),
                    level["mbps"].as_f64().unwrap_or(0.0),
                    level["speedup_vs_base"].as_f64().unwrap_or(1.0),
                );
            }
            crate::outln!(
                "payload hashes consistent: {}",
                if hash_consistent { "yes" } else { "NO" }
            );
            crate::outln!("machine output: rdm bench --output json");
        }
    }

    if !hash_consistent {
        let error = rdm_agent::AgentErrorInfo::new(
            rdm_agent::ErrorCode::Internal,
            "benchmark payloads are not identical across connection levels",
        )
        .with_hint("this is an engine bug; collect `rdm selftest --output json` + `rdm bench --output json`");
        reporter.finish_report(data, Some(error))
    } else {
        reporter.finish_report(data, None)
    }
}

fn median(sorted: &[u64]) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    }
}

fn parse_connection_levels(raw: &str) -> Result<Vec<usize>, String> {
    let mut levels = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let connections: usize = item
            .parse()
            .map_err(|_| format!("invalid --connections item `{item}` (expected a number)"))?;
        if connections == 0 || connections > rdm_core::MAX_CONNECTIONS {
            return Err(format!(
                "connection level {connections} is outside 1..={}",
                rdm_core::MAX_CONNECTIONS
            ));
        }
        if !levels.contains(&connections) {
            levels.push(connections);
        }
    }
    if levels.is_empty() {
        return Err("--connections needs at least one level, e.g. `1,8`".to_string());
    }
    levels.sort_unstable();
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_connection_levels() {
        assert_eq!(parse_connection_levels("8,1,8").unwrap(), vec![1, 8]);
        assert_eq!(parse_connection_levels(" 2 , 4 ").unwrap(), vec![2, 4]);
        assert!(parse_connection_levels("0").is_err());
        assert!(parse_connection_levels("33").is_err());
        assert!(parse_connection_levels("").is_err());
        assert!(parse_connection_levels("x").is_err());
    }

    #[test]
    fn computes_medians() {
        assert_eq!(median(&[3]), 3);
        assert_eq!(median(&[1, 3]), 2);
        assert_eq!(median(&[1, 2, 9]), 2);
        assert_eq!(median(&[]), 0);
    }
}
