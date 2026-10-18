//! 本机 IPC server（GUI 侧）：`127.0.0.1:0` 随机端口 + 随机 token + JSONL 一问一答。
//!
//! 协议见 `docs/extension-protocol.md` §2 与 `crates/rdm-ipc/src/lib.rs`：
//! - 启动顺序（宿主依赖）：bind → `rdm_ipc::write_info` → 处理 spool/请求；
//! - 每连接一行 `Envelope` 请求 → 一行 `Response` 响应；
//! - token 不匹配回 `unauthorized` 并断开；
//! - 退出时删除 `ipc.json`（`cleanup_info_file`）。
//!
//! 真正的业务处理在 GUI 主线程（`RdmApp::handle_ipc`）：这里只做鉴权、解析、
//! 转发到 `EngineMsg::Ipc` 并等待 `oneshot` 回包，不直接触碰 `Entity`。

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rdm_ipc::{Envelope, IpcInfo, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::EngineMsg;
use crate::model::{active_count, global_speed_bps, DownloadItem};

/// 单连接读/写超时（与协议文档一致：5s）。
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// 32 字节（64 hex 字符）的进程内随机 token。
///
/// 用 `std::collections::hash_map::RandomState`（由 OS 熵播种的 per-instance 随机键）
/// 生成 4 个 u64，避免为此引入 `rand` 依赖。token 只用于 loopback 认证。
pub fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut words = [0u64; 4];
    for (index, slot) in words.iter_mut().enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(index);
        hasher.write_u32(std::process::id());
        hasher.write_u128(nanos_since_epoch());
        *slot = hasher.finish();
    }
    token_from_words(words)
}

/// 由 4 个 u64 拼出 64 hex 字符（测试用可控输入）。
pub fn token_from_words(words: [u64; 4]) -> String {
    let mut token = String::with_capacity(64);
    for word in words {
        token.push_str(&format!("{word:016x}"));
    }
    token
}

fn nanos_since_epoch() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 绑定 loopback 随机端口并构造要发布的 `IpcInfo`。
pub async fn bind(token: String) -> io::Result<(TcpListener, IpcInfo)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let info = IpcInfo {
        protocol: rdm_ipc::PROTOCOL_VERSION,
        port,
        token,
        pid: std::process::id(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    Ok((listener, info))
}

/// 原子发布 `ipc.json`，返回其路径。
pub fn publish(info: &IpcInfo) -> Result<PathBuf, rdm_ipc::IpcError> {
    rdm_ipc::write_info(info)?;
    Ok(rdm_ipc::info_path())
}

/// 退出时删除 `ipc.json`（尽力而为；token 已随进程失效，残留也连不上）。
pub fn cleanup_info_file() {
    let path = rdm_ipc::info_path();
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "已删除 ipc.json"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(path = %path.display(), error = %err, "删除 ipc.json 失败"),
    }
}

/// accept 循环。每个连接独立 task；请求经 `EngineMsg::Ipc` 投递给 GUI。
pub async fn serve(listener: TcpListener, token: String, tx: UnboundedSender<EngineMsg>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let token = token.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, token, tx).await {
                        tracing::debug!(%peer, error = %err, "IPC 连接处理失败");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "IPC accept 失败");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// 处理单条连接：读一行 → 鉴权 → 转发 GUI → 写一行响应。
pub async fn handle_connection(
    stream: TcpStream,
    token: String,
    tx: UnboundedSender<EngineMsg>,
) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    let read = tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line)).await;
    match read {
        Ok(Ok(0)) => return Ok(()),
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            write_response(&mut write_half, &Response::error(0, "timeout")).await?;
            return Ok(());
        }
    }

    let envelope = match parse_envelope(&line) {
        Ok(envelope) => envelope,
        Err(err) => {
            write_response(&mut write_half, &Response::error(0, err)).await?;
            return Ok(());
        }
    };

    if !authorized(&envelope, &token) {
        write_response(
            &mut write_half,
            &Response::error(envelope.id, "unauthorized"),
        )
        .await?;
        return Ok(());
    }

    let request_id = envelope.id;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if tx
        .send(EngineMsg::Ipc {
            envelope,
            reply: reply_tx,
        })
        .is_err()
    {
        write_response(
            &mut write_half,
            &Response::error(request_id, "internal: gui is shutting down"),
        )
        .await?;
        return Ok(());
    }

    let response = match tokio::time::timeout(IO_TIMEOUT, reply_rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => Response::error(request_id, "internal: request dropped"),
        Err(_) => Response::error(request_id, "timeout"),
    };
    write_response(&mut write_half, &response).await?;
    Ok(())
}

/// 解析一行 `Envelope` JSON。
///
/// 注意 `rdm_ipc::Envelope` 的线格式有一个坑：`#[serde(flatten)]` 的
/// `Request::Pause { id }` 等控制方法与 `Envelope.id`（u64）**都叫 `id`**，
/// 序列化后是 `{"id":4,"token":"..","method":"pause","id":"dl_01"}` 这种重复键；
/// serde flatten 反序列化会因类型冲突失败。这里对控制方法做宽容解析：
/// 从原文取第一个数字 `id` 作为信封 id，从 JSON 对象取（最后出现的）字符串
/// `id` 作为 download id，再手工构造 `Envelope`。
pub fn parse_envelope(line: &str) -> Result<Envelope, String> {
    let line = line.trim();
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("invalid_request: {err}"))?;
    let method = value
        .get("method")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "invalid_request: missing method".to_string())?;
    let token = value
        .get("token")
        .and_then(|value| value.as_str())
        .map(str::to_owned);

    if matches!(method, "pause" | "resume" | "cancel" | "open_folder") {
        let download_id = value
            .get("id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "invalid_request: missing download id".to_string())?
            .to_string();
        let envelope_id = first_numeric_id(line).unwrap_or(0);
        let request = match method {
            "pause" => Request::Pause { id: download_id },
            "resume" => Request::Resume { id: download_id },
            "cancel" => Request::Cancel { id: download_id },
            _ => Request::OpenFolder { id: download_id },
        };
        return Ok(Envelope {
            id: envelope_id,
            token,
            request,
        });
    }

    serde_json::from_str::<Envelope>(line).map_err(|err| format!("invalid_request: {err}"))
}

/// 在原始 JSON 行里找第一个 `"id": <数字>`（控制方法的外层信封 id）。
fn first_numeric_id(line: &str) -> Option<u64> {
    let bytes = line.as_bytes();
    let needle = b"\"id\"";
    let mut search_from = 0usize;
    while search_from + needle.len() <= bytes.len() {
        let window = &bytes[search_from..];
        let position = window
            .windows(needle.len())
            .position(|part| part == needle)?;
        let mut index = search_from + position + needle.len();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index < bytes.len() && bytes[index] == b':' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            let digits_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index > digits_start {
                return line[digits_start..index].parse::<u64>().ok();
            }
        }
        search_from += position + 1;
    }
    None
}

fn authorized(envelope: &Envelope, expected: &str) -> bool {
    envelope
        .token
        .as_deref()
        .is_some_and(|token| token == expected)
}

async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Response,
) -> io::Result<()> {
    let mut line = serde_json::to_string(response)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

// ---------------------------------------------------------------------------
// `status` / `ping` 的响应负载（纯函数，可单测）
// ---------------------------------------------------------------------------

pub fn status_payload(items: &[DownloadItem]) -> serde_json::Value {
    let downloads: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "file_name": item.file_name,
                "state": item.state.wire_name(),
                "downloaded": item.downloaded,
                "total": item.total,
                "speed_bps": item.speed_bps,
                "connections": item.request.connections,
            })
        })
        .collect();
    serde_json::json!({
        "downloads": downloads,
        "global_speed_bps": global_speed_bps(items),
        "active": active_count(items),
    })
}

pub fn ping_payload(items: &[DownloadItem]) -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": rdm_ipc::PROTOCOL_VERSION,
        "downloads": items.len(),
        "active": active_count(items),
        "gui_running": true,
    })
}

/// 构造 `enqueue` 成功的响应数据。
pub fn enqueue_payload(download_id: &str) -> serde_json::Value {
    serde_json::json!({ "download_id": download_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{next_download_id, DownloadItem, ItemState};
    use rdm_core::DownloadRequest;
    use rdm_ipc::Request;

    #[test]
    fn token_is_64_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, generate_token());
    }

    #[test]
    fn token_from_words_is_padded_hex() {
        assert_eq!(
            token_from_words([1, 2, 3, 4]),
            "0000000000000001000000000000000200000000000000030000000000000004"
        );
    }

    #[test]
    fn authorizes_matching_token_only() {
        let mut envelope = Envelope {
            id: 1,
            token: Some("abc".to_string()),
            request: Request::Ping,
        };
        assert!(authorized(&envelope, "abc"));
        assert!(!authorized(&envelope, "abd"));
        envelope.token = None;
        assert!(!authorized(&envelope, "abc"));
    }

    #[test]
    fn parses_envelopes() {
        let line = r#"{"id":7,"token":"t","method":"ping"}"#;
        let envelope = parse_envelope(line).unwrap();
        assert_eq!(envelope.id, 7);
        assert!(matches!(envelope.request, Request::Ping));

        assert!(parse_envelope("not json").is_err());
        assert!(parse_envelope(r#"{"token":"t","method":"ping"}"#).is_err());
    }

    #[test]
    fn parses_real_rdm_ipc_envelope_serialization() {
        // 用 rdm_ipc 自身的 Envelope 序列化，确认 parse_envelope 能还原（含重复 id）。
        for request in [
            Request::Pause {
                id: "dl_000009".into(),
            },
            Request::Resume {
                id: "dl_000009".into(),
            },
            Request::Cancel {
                id: "dl_000009".into(),
            },
            Request::OpenFolder {
                id: "dl_000009".into(),
            },
            Request::Ping,
            Request::Status,
            Request::Activate,
        ] {
            let envelope = Envelope {
                id: 9,
                token: Some("tok".into()),
                request,
            };
            let line = serde_json::to_string(&envelope).unwrap();
            let parsed =
                parse_envelope(&line).unwrap_or_else(|err| panic!("解析 {line} 失败: {err}"));
            assert_eq!(parsed.id, 9, "line={line}");
            assert_eq!(parsed.token.as_deref(), Some("tok"));
        }
    }

    #[test]
    fn parses_control_methods_with_duplicate_id_keys() {
        // 与 rdm_ipc::Envelope 的实际线格式一致：外层 id 数字 + 内层 download id 字符串，
        // 序列化后是重复的 "id" 键（详见 parse_envelope 文档注释）。
        let cases = [
            (
                "pause",
                r#"{"id":4,"token":"tok","method":"pause","id":"dl_01"}"#,
            ),
            (
                "resume",
                r#"{"id":5,"token":"tok","method":"resume","id":"dl_02"}"#,
            ),
            (
                "cancel",
                r#"{"id":6,"token":"tok","method":"cancel","id":"dl_03"}"#,
            ),
            (
                "open_folder",
                r#"{"id":7,"token":"tok","method":"open_folder","id":"dl_04"}"#,
            ),
        ];
        for (index, (method, line)) in cases.into_iter().enumerate() {
            let envelope =
                parse_envelope(line).unwrap_or_else(|err| panic!("{method} 解析失败: {err}"));
            assert_eq!(envelope.id, 4 + index as u64, "{method} 信封 id");
            assert_eq!(envelope.token.as_deref(), Some("tok"));
            let download_id = match envelope.request {
                Request::Pause { id }
                | Request::Resume { id }
                | Request::Cancel { id }
                | Request::OpenFolder { id } => id,
                other => panic!("{method} 解析成了错误变体: {other:?}"),
            };
            assert_eq!(download_id, format!("dl_{:02}", index + 1));
        }
    }

    #[test]
    fn parses_enqueue_like_rdm_ipc() {
        let line = r#"{"id":2,"token":"tok","method":"enqueue","item":{"download":{"url":"https://example.com/a.bin"},"source":"firefox","source_page":null}}"#;
        let envelope = parse_envelope(line).unwrap();
        assert_eq!(envelope.id, 2);
        match envelope.request {
            Request::Enqueue { item } => {
                assert_eq!(item.download.url, "https://example.com/a.bin");
                assert_eq!(item.source, "firefox");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn first_numeric_id_skips_string_ids() {
        assert_eq!(first_numeric_id(r#"{"id":"dl_01"}"#), None);
        assert_eq!(first_numeric_id(r#"{"id": 12}"#), Some(12));
        assert_eq!(
            first_numeric_id(r#"{"id":3,"token":"t","method":"pause","id":"dl_01"}"#),
            Some(3)
        );
        assert_eq!(first_numeric_id(r#"{"token":"a\"id\":1","id":9}"#), Some(9));
    }

    #[test]
    fn status_payload_shape() {
        let mut item = DownloadItem::new(
            next_download_id(1),
            DownloadRequest::new("https://example.com/a.bin"),
            "test",
        );
        item.state = ItemState::Downloading;
        item.downloaded = 10;
        item.total = Some(100);
        item.speed_bps = 5.0;
        let payload = status_payload(&[item]);
        assert_eq!(payload["global_speed_bps"], 5.0);
        assert_eq!(payload["active"], 1);
        assert_eq!(payload["downloads"][0]["id"], "dl_000001");
        assert_eq!(payload["downloads"][0]["state"], "downloading");
        assert_eq!(payload["downloads"][0]["total"], 100);
        assert_eq!(
            payload["downloads"][0]["connections"],
            rdm_core::DEFAULT_CONNECTIONS
        );
    }

    #[test]
    fn ping_payload_has_protocol_and_version() {
        let payload = ping_payload(&[]);
        assert_eq!(payload["protocol"], rdm_ipc::PROTOCOL_VERSION);
        assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(payload["gui_running"], true);
    }

    #[test]
    fn enqueue_payload_has_download_id() {
        assert_eq!(enqueue_payload("dl_000003")["download_id"], "dl_000003");
    }

    #[test]
    fn response_serializes_with_newline() {
        let response = Response::ok(3, serde_json::json!({"download_id": "dl_000003"}));
        let line = serde_json::to_string(&response).unwrap();
        assert!(line.contains("\"ok\":true"));
        assert!(line.contains("dl_000003"));
    }
}
