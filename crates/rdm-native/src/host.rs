//! The native messaging host event loop: stdin frames in, stdout frames out.
//!
//! One request/one response, strictly sequential (Firefox itself multiplexes
//! over the single port; we simply answer in order). Every error path tries to
//! answer with a structured `error` message — a silent disconnect makes the
//! Firefox side report a useless "native host exited" error instead of a
//! diagnosable reason.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::framing::{self, FrameError};
use crate::gui;
use crate::ipc_client;
use crate::logger::Logger;
use crate::protocol::{self, codes, Incoming, Outgoing};
use crate::spool;

/// Run the host until stdin is closed.
///
/// Returns `Err` only for an unexpected I/O failure while reading the channel;
/// a closed pipe (Firefox disconnected) is a clean shutdown and returns `Ok`.
pub fn run<R: Read, W: Write>(
    input: R,
    output: W,
    logger: &Logger,
    state_dir: &Path,
) -> Result<(), FrameError> {
    let mut reader = BufReader::new(input);
    let mut writer = BufWriter::new(output);
    loop {
        match framing::read_frame(&mut reader) {
            Ok(None) => {
                logger.info("stdin closed; native host exiting");
                return Ok(());
            }
            Ok(Some(frame)) => {
                if frame.is_empty() {
                    logger.warn("received an empty message frame");
                    let response = Outgoing::error(0, codes::INVALID_REQUEST, "empty message");
                    if write_response(&mut writer, &response, logger).is_err() {
                        return Ok(());
                    }
                    continue;
                }
                let response = handle_message(&frame, logger, state_dir);
                if write_response(&mut writer, &response, logger).is_err() {
                    return Ok(());
                }
            }
            Err(FrameError::Io(err)) => {
                // Firefox closing the pipe is the normal way a host exits.
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                ) {
                    logger.info("native messaging channel closed; exiting");
                    return Ok(());
                }
                logger.error(&format!(
                    "fatal error reading native messaging channel: {err}"
                ));
                return Err(FrameError::Io(err));
            }
            Err(FrameError::TooLarge(len)) => {
                // We cannot resynchronise a stream whose length prefix lies,
                // so answer once and close.
                logger.error(&format!(
                    "inbound message length {len} exceeds the 1 MiB limit; closing channel"
                ));
                let response =
                    Outgoing::error(0, codes::INVALID_REQUEST, "message exceeds 1 MiB limit");
                let _ = write_response(&mut writer, &response, logger);
                return Ok(());
            }
            Err(err @ FrameError::Truncated(_)) => {
                logger.error(&format!("truncated native message: {err}"));
                return Ok(());
            }
        }
    }
}

fn handle_message(frame: &[u8], logger: &Logger, state_dir: &Path) -> Outgoing {
    let parsed: serde_json::Value = match serde_json::from_slice(frame) {
        Ok(value) => value,
        Err(err) => {
            logger.warn(&format!("invalid JSON from extension: {err}"));
            return Outgoing::error(0, codes::INVALID_REQUEST, format!("invalid JSON: {err}"));
        }
    };
    let fallback_id = protocol::extract_id(&parsed);
    let incoming: Incoming = match serde_json::from_value(parsed) {
        Ok(incoming) => incoming,
        Err(err) => {
            logger.warn(&format!("invalid message from extension: {err}"));
            return Outgoing::error(
                fallback_id,
                codes::INVALID_REQUEST,
                format!("invalid message: {err}"),
            );
        }
    };
    match incoming {
        Incoming::Ping { id } => handle_ping(id.unwrap_or(fallback_id), logger),
        Incoming::Status { id } => handle_status(id.unwrap_or(fallback_id), logger),
        Incoming::Enqueue {
            id,
            request,
            launch_gui,
        } => handle_enqueue(
            id.unwrap_or(fallback_id),
            *request,
            launch_gui,
            logger,
            state_dir,
        ),
        Incoming::AiParse {
            id,
            url,
            source_page,
            title,
            include_credentials,
            cookie,
            referer,
            user_agent,
            page_url,
            page_title,
            page_links,
            launch_gui,
        } => handle_ai_parse(
            id.unwrap_or(fallback_id),
            protocol::AiParseRequest {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
            },
            launch_gui,
            logger,
            state_dir,
        ),
        Incoming::Unknown => {
            Outgoing::error(fallback_id, codes::INVALID_REQUEST, "unknown message type")
        }
    }
}

fn handle_ping(id: u64, logger: &Logger) -> Outgoing {
    match ipc_client::ping() {
        Ok(()) => {
            logger.info("ping: RDM GUI online");
            Outgoing::pong(id, true)
        }
        Err(err) => {
            logger.info(&format!("ping: RDM GUI offline ({err})"));
            Outgoing::pong(id, false)
        }
    }
}

/// Handle `status`: read-only progress query.
///
/// Deliberately does **not** go through `gui::ensure_gui` and never touches the
/// spool: the extension polls status repeatedly (about once per second while
/// its popup is open), so a missing GUI must not spawn `rdm-gui.exe` and must
/// not answer with an `error` frame. The degraded answer is an `ok:true` frame
/// with `gui_running:false` and an empty snapshot, plus one warning log line.
fn handle_status(id: u64, logger: &Logger) -> Outgoing {
    match ipc_client::status() {
        Ok(snapshot) => status_frame(id, Ok(snapshot)),
        Err(err) => {
            logger.warn(&format!(
                "status: RDM GUI unavailable ({err}); returning an empty snapshot"
            ));
            status_frame(id, Err(err))
        }
    }
}

/// Pure mapping from an IPC status result to the extension-facing frame.
///
/// Split out so the three meaningful outcomes — GUI online, GUI not running,
/// GUI rejecting the query — are unit-testable without a live GUI. Every
/// outcome is a successful `status` frame; only genuinely unexpected internal
/// failures elsewhere in the host use `error` / `code:"internal"`.
fn status_frame(
    id: u64,
    result: Result<ipc_client::StatusSnapshot, ipc_client::IpcCallError>,
) -> Outgoing {
    match result {
        Ok(snapshot) => Outgoing::status(id, true, snapshot),
        Err(_) => Outgoing::status(id, false, ipc_client::StatusSnapshot::default()),
    }
}

fn handle_enqueue(
    id: u64,
    request: protocol::ExtensionRequest,
    launch_gui: bool,
    logger: &Logger,
    state_dir: &Path,
) -> Outgoing {
    let item = match request.validate(logger) {
        Ok(item) => item,
        Err(message) => {
            logger.warn(&format!("rejected enqueue: {message}"));
            return Outgoing::error(id, codes::INVALID_REQUEST, message);
        }
    };

    let gui_status = gui::ensure_gui(launch_gui, logger, state_dir);
    if !gui_status.running {
        let reason = gui_status
            .reason
            .unwrap_or_else(|| "gui_unavailable".to_string());
        logger.warn(&format!(
            "RDM GUI unavailable ({reason}); spooling the download instead"
        ));
        return spool_and_reply(id, &item, codes::GUI_UNAVAILABLE, logger, state_dir);
    }

    match ipc_client::enqueue(item.clone()) {
        Ok(download_id) => {
            logger.info(&format!(
                "handoff accepted by GUI (download_id={})",
                if download_id.is_empty() {
                    "<none>"
                } else {
                    download_id.as_str()
                }
            ));
            Outgoing::enqueued(id, download_id)
        }
        Err(err) if err.is_rejection() => {
            let code = rejection_code(&err);
            logger.warn(&format!("GUI rejected handoff ({code}): {err}"));
            Outgoing::error(id, code, err.to_string())
        }
        Err(err) => {
            // The GUI died between the ping and the handoff. Losing the
            // download would be the worst outcome, so spool it.
            logger.warn(&format!(
                "IPC handoff failed after GUI was reported online ({err}); spooling"
            ));
            spool_and_reply(id, &item, codes::GUI_UNAVAILABLE, logger, state_dir)
        }
    }
}

/// Handle `ai_parse`: validate, make sure the GUI is available, then forward
/// `rdm_ipc::Request::AiParse`.
///
/// AI parsing is deliberately **not spooled**: the result must appear in the
/// live GUI AI panel, and the request may carry a session cookie that must
/// never be written to disk. A missing GUI therefore becomes a structured
/// `gui_unavailable` error instead of a `queued` response.
fn handle_ai_parse(
    id: u64,
    request: protocol::AiParseRequest,
    launch_gui: bool,
    logger: &Logger,
    state_dir: &Path,
) -> Outgoing {
    let request = match request.validate(logger) {
        Ok(request) => request,
        Err(message) => {
            logger.warn(&format!("rejected ai_parse: {message}"));
            return Outgoing::error(id, codes::INVALID_REQUEST, message);
        }
    };

    let gui_status = gui::ensure_gui(launch_gui, logger, state_dir);
    if !gui_status.running {
        let reason = gui_status
            .reason
            .unwrap_or_else(|| "gui_unavailable".to_string());
        logger.warn(&format!(
            "RDM GUI unavailable ({reason}); AI parse cannot be queued"
        ));
        return Outgoing::error(
            id,
            codes::GUI_UNAVAILABLE,
            "RDM GUI is not running; AI 解析需要桌面端在线（不会排队）",
        );
    }

    match ipc_client::ai_parse(&request) {
        Ok(request_id) => {
            logger.info(&format!(
                "AI parse accepted by GUI (request_id={request_id})"
            ));
            Outgoing::ai_parse_accepted(id, request_id, true)
        }
        Err(err) if err.is_rejection() => {
            let code = rejection_code(&err);
            logger.warn(&format!("GUI rejected AI parse ({code}): {err}"));
            Outgoing::error(id, code, err.to_string())
        }
        Err(err) => {
            // The GUI died between the liveness probe and the handoff. There is
            // nothing safe to persist, so report it as unavailable.
            logger.warn(&format!(
                "IPC ai_parse failed after GUI was reported online ({err})"
            ));
            Outgoing::error(
                id,
                codes::GUI_UNAVAILABLE,
                "RDM GUI connection failed; AI 解析未排队",
            )
        }
    }
}

fn rejection_code(err: &ipc_client::IpcCallError) -> &'static str {
    let lower = err.to_string().to_ascii_lowercase();
    if lower.contains("unauthorized") {
        codes::UNAUTHORIZED
    } else if lower.contains("duplicate") {
        codes::DUPLICATE
    } else {
        codes::INVALID_REQUEST
    }
}

fn spool_and_reply(
    id: u64,
    item: &rdm_ipc::EnqueueItem,
    reason: &str,
    logger: &Logger,
    state_dir: &Path,
) -> Outgoing {
    match spool::spool_item(state_dir, item) {
        Ok(spooled) => {
            logger.info(&format!(
                "spooled handoff to {} (credentials_stripped={})",
                spooled.path.display(),
                spooled.credentials_stripped
            ));
            Outgoing::queued(id, reason, &spooled.path, spooled.credentials_stripped)
        }
        Err(err) => {
            logger.error(&format!("failed to spool handoff: {err}"));
            Outgoing::error(
                id,
                codes::INTERNAL,
                format!("failed to spool download: {err}"),
            )
        }
    }
}

fn write_response<W: Write>(
    writer: &mut W,
    response: &Outgoing,
    logger: &Logger,
) -> Result<(), FrameError> {
    let payload = response.to_json();
    match framing::write_frame(writer, &payload) {
        Ok(()) => Ok(()),
        Err(err) => {
            if let FrameError::Io(io_err) = &err {
                if matches!(
                    io_err.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                ) {
                    logger.info("extension closed the channel before the response was written");
                    return Ok(());
                }
            }
            logger.error(&format!("failed to write response: {err}"));
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_logger(tag: &str) -> (Logger, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "rdm-native-host-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Logger::new(&dir), dir)
    }

    fn run_with_input(input: Vec<u8>, tag: &str) -> (Vec<Vec<u8>>, std::path::PathBuf) {
        let (logger, dir) = test_logger(tag);
        let mut output = Vec::new();
        run(std::io::Cursor::new(input), &mut output, &logger, &dir)
            .expect("host loop must not fail on these inputs");
        let mut frames = Vec::new();
        let mut cursor = std::io::Cursor::new(output);
        while let Some(frame) = framing::read_frame(&mut cursor).unwrap() {
            frames.push(frame);
        }
        (frames, dir)
    }

    fn encode(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for payload in payloads {
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn invalid_json_gets_structured_error_and_host_continues() {
        let input = encode(&[b"not json", br#"{"type":"mystery","id":5}"#]);
        let (frames, dir) = run_with_input(input, "invalid-json");
        assert_eq!(frames.len(), 2);
        let first: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(first["type"], "error");
        assert_eq!(first["code"], "invalid_request");
        let second: serde_json::Value = serde_json::from_slice(&frames[1]).unwrap();
        assert_eq!(second["type"], "error");
        assert_eq!(second["id"], 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_frame_gets_error() {
        let input = 0u32.to_le_bytes().to_vec();
        let (frames, dir) = run_with_input(input, "empty");
        assert_eq!(frames.len(), 1);
        let value: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "invalid_request");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_frame_is_answered_then_channel_closes() {
        let mut input = (framing::MAX_FRAME_BYTES + 1).to_le_bytes().to_vec();
        input.extend_from_slice(&[0u8; 16]);
        let (frames, dir) = run_with_input(input, "oversized");
        assert_eq!(frames.len(), 1);
        let value: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(value["type"], "error");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_frame_exits_without_crashing() {
        let mut input = 10u32.to_le_bytes().to_vec();
        input.extend_from_slice(b"abc");
        let (frames, dir) = run_with_input(input, "truncated");
        assert!(frames.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejection_code_mapping() {
        use crate::ipc_client::IpcCallError;
        assert_eq!(
            rejection_code(&IpcCallError::Rejected("unauthorized token".into())),
            codes::UNAUTHORIZED
        );
        assert_eq!(
            rejection_code(&IpcCallError::Rejected("duplicate download".into())),
            codes::DUPLICATE
        );
        assert_eq!(
            rejection_code(&IpcCallError::Rejected("invalid url".into())),
            codes::INVALID_REQUEST
        );
    }

    // -- status -> frame mapping (pure) ------------------------------------

    fn assert_degraded_status_frame(frame: Outgoing, expected_id: u64) {
        match frame {
            Outgoing::Status {
                id,
                ok,
                gui_running,
                active,
                global_speed_bps,
                downloads,
            } => {
                assert_eq!(id, expected_id);
                assert!(ok, "status is a poll: every outcome must be ok:true");
                assert!(!gui_running, "an IPC failure must report gui_running:false");
                assert_eq!(active, 0);
                assert_eq!(global_speed_bps, 0.0);
                assert!(downloads.is_empty());
            }
            other => panic!("expected a status frame, got {other:?}"),
        }
    }

    #[test]
    fn status_frame_for_online_gui_keeps_snapshot() {
        let snapshot = ipc_client::StatusSnapshot {
            active: 2,
            global_speed_bps: 1_048_576.0,
            downloads: vec![crate::protocol::DownloadStatus {
                id: "dl_000001".to_string(),
                file_name: "a.bin".to_string(),
                state: "downloading".to_string(),
                downloaded: 1_048_576,
                total: Some(10_485_760),
                speed_bps: 524_288.0,
                connections: 8,
                active_connections: 6,
                target_connections: 12,
                peak_connections: 16,
            }],
        };
        match status_frame(51, Ok(snapshot)) {
            Outgoing::Status {
                id,
                ok,
                gui_running,
                active,
                global_speed_bps,
                downloads,
            } => {
                assert_eq!(id, 51);
                assert!(ok);
                assert!(gui_running);
                assert_eq!(active, 2);
                assert_eq!(global_speed_bps, 1_048_576.0);
                assert_eq!(downloads.len(), 1);
                assert_eq!(downloads[0].file_name, "a.bin");
                assert_eq!(downloads[0].total, Some(10_485_760));
                assert_eq!(downloads[0].speed_bps, 524_288.0);
                assert_eq!(
                    downloads[0].active_connections, 6,
                    "the frame must forward the GUI's active worker count"
                );
                assert_eq!(downloads[0].target_connections, 12);
                assert_eq!(downloads[0].peak_connections, 16);
            }
            other => panic!("expected a status frame, got {other:?}"),
        }
    }

    #[test]
    fn status_frame_degrades_when_gui_is_not_running() {
        use crate::ipc_client::IpcCallError;
        let frame = status_frame(52, Err(IpcCallError::NotRunning("no ipc.json".into())));
        assert_degraded_status_frame(frame, 52);
    }

    #[test]
    fn status_frame_degrades_when_gui_rejects_the_query() {
        use crate::ipc_client::IpcCallError;
        // A rejected status query (e.g. token mismatch) must not become an
        // error frame either: the extension has no recovery action while
        // polling, so degrading keeps its UI stable.
        let frame = status_frame(53, Err(IpcCallError::Rejected("unauthorized".into())));
        assert_degraded_status_frame(frame, 53);
    }

    #[test]
    fn status_frame_degrades_on_protocol_and_transport_errors() {
        use crate::ipc_client::IpcCallError;
        for err in [
            IpcCallError::Protocol("invalid response".into()),
            IpcCallError::Transport("connection reset".into()),
        ] {
            assert_degraded_status_frame(status_frame(54, Err(err)), 54);
        }
    }

    #[test]
    fn ai_parse_invalid_url_is_rejected_without_touching_gui_or_spooling() {
        let input = encode(&[br#"{"type":"ai_parse","id":3,"url":"file:///C:/Windows/win.ini"}"#]);
        let (frames, dir) = run_with_input(input, "ai-parse-invalid");
        assert_eq!(frames.len(), 1);
        let value: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "invalid_request");
        assert_eq!(value["id"], 3);
        assert!(
            !dir.join("queue").exists(),
            "AI parse failures must never spool to disk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ai_parse_missing_url_is_a_serde_error_answered_with_error_frame() {
        // `url` is the only required ai_parse field; a missing one is a
        // structured error (with the fallback id), not a silent disconnect.
        let input = encode(&[br#"{"type":"ai_parse","id":4}"#]);
        let (frames, dir) = run_with_input(input, "ai-parse-no-url");
        assert_eq!(frames.len(), 1);
        let value: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "invalid_request");
        assert_eq!(value["id"], 4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
