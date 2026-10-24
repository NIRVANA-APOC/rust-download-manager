//! P5：GUI AI 解析的 **resolver-first** 结果映射（`docs/architecture.md` §8.5）。
//!
//! `rdm-resolve` 的 [`rdm_resolve::ResolveOutcome`] 与 GUI/native/扩展共用的
//! [`rdm_ai::ParseReport`] 是两套契约；本模块只做**纯映射**（无 IO、无网络、无缓存），
//! 把「至少 1 个可下载资源」的 resolver 结果投影成既有 ParseReport，保证
//! app/model/native 完全不需要改动。
//!
//! 解析不了（无 verified 资源、失败、`model_unavailable`）**或**需要凭据时，由
//! [`crate::engine::EngineRuntime::start_ai_parse`] 走 resolver/legacy 降级链：
//! 带 Cookie 的请求会先构造仅内存的 `HostCredentials` 交给 resolver 验证/方法回放，
//! 仍没有 verified 资源才降级到 legacy `rdm_ai::parse_url`（P4a 后的真实语义，
//! 旧注释「带 Cookie 时直接降级 legacy」已过时——审计 F-22）。

use rdm_ai::{ParseReport, ParsedResource};

/// Resolver-first 成功时最多映射的资源数（与 `ResolveRequest.max_resources` 对齐）。
const MAX_MAPPED_RESOURCES: usize = 8;
/// `diagnostics.summary` 写入 notes 时的字符上限。
const SUMMARY_MAX_CHARS: usize = 512;
/// ParseReport 的 notes 上限（契约 §8.5：1–8 条）。
const MAX_NOTES: usize = 8;
/// manifest.notes 最多带几条（留出摘要位置）。
const MAX_MANIFEST_NOTES: usize = 6;

/// 把一次 resolver 结果映射为既有 [`ParseReport`]。
///
/// 只有当 `outcome.manifest` 至少含 1 个 `verified`/`head_only_verified` 资源时返回
/// `Some`；其余情况返回 `None`，由调用方降级 legacy。
///
/// - `source_kind = direct_file`：单资源、无模型调用、无 recipe，且 final/资源 URL 就是
///   请求 URL（内置直链验证路径）；否则 `html_page`（方法回放或探索）。
/// - `ai_used = diagnostics.llm_calls > 0`；`model` 只在真正调用模型时保留。
/// - 不写解析缓存（`cache_hit=false`、`failure_kind=None`）：resolver 有自己的轨迹，
///   GUI 旧缓存只在 legacy 路径维护。
pub fn report_from_outcome(
    request_url: &str,
    request_title: Option<&str>,
    model: Option<&str>,
    outcome: &rdm_resolve::ResolveOutcome,
) -> Option<ParseReport> {
    let manifest = outcome.manifest.as_ref()?;
    let request_url = request_url.trim();

    let resources: Vec<ParsedResource> = manifest
        .downloadable()
        .take(MAX_MAPPED_RESOURCES)
        .map(|item| {
            let name = item
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| file_name_from_url(&item.url));
            let kind = item
                .kind
                .clone()
                .filter(|kind| !kind.trim().is_empty())
                .unwrap_or_else(|| {
                    kind_from_category(&rdm_ai::classify::category_for(
                        &name,
                        item.content_type.as_deref(),
                    ))
                    .to_owned()
                });
            // Defense in depth: resolver manifests are normalized already, but
            // older cached manifests / legacy reports may still carry a bare
            // page title. Never hand the GUI a task name without a suffix.
            let name = rdm_ai::ensure_file_extension(
                &name,
                item.content_type.as_deref(),
                Some(&item.url),
                Some(&kind),
            );
            ParsedResource {
                url: item.url.clone(),
                name,
                kind,
                size_bytes: item.size_bytes,
                content_type: item.content_type.clone(),
                note: Some(verification_note(item.verification.status)),
            }
        })
        .collect();
    if resources.is_empty() {
        return None;
    }

    let ai_used = outcome.diagnostics.llm_calls > 0;
    let source_kind = if resources.len() == 1
        && !ai_used
        && manifest.source.recipe_id.is_none()
        && (manifest.source.final_url.as_deref() == Some(request_url)
            || resources[0].url == request_url)
    {
        "direct_file"
    } else {
        "html_page"
    };

    let mut notes: Vec<String> = manifest
        .notes
        .iter()
        .map(|note| note.trim())
        .filter(|note| !note.is_empty())
        .take(MAX_MANIFEST_NOTES)
        .map(str::to_owned)
        .collect();
    let summary = truncate_chars(outcome.diagnostics.summary.trim(), SUMMARY_MAX_CHARS);
    if !summary.is_empty() {
        notes.push(summary);
    }
    notes.truncate(MAX_NOTES);

    let source_url = if manifest.source.input_url.trim().is_empty() {
        request_url.to_owned()
    } else {
        manifest.source.input_url.clone()
    };
    let model = if ai_used {
        model
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    Some(ParseReport {
        source_url,
        source_kind: source_kind.to_owned(),
        title: request_title.map(str::to_owned),
        resources,
        notes,
        ai_used,
        model,
        usage: crate::ai_resolve::token_usage_from_diagnostics(&outcome.diagnostics),
        created_ms: crate::model::now_ms(),
        cache_hit: false,
        failure_kind: None,
    })
}

/// Resolver 诊断里的 LLM 用量 → `ParseReport.usage`（资源监控记账用）。
///
/// 字段由 `rdm-resolve` 的 `Diagnostics` 提供；旧版本诊断为 0 时保持零值。
fn token_usage_from_diagnostics(diagnostics: &rdm_resolve::Diagnostics) -> rdm_ai::TokenUsage {
    rdm_ai::TokenUsage {
        prompt_tokens: diagnostics.llm_prompt_tokens,
        completion_tokens: diagnostics.llm_completion_tokens,
        total_tokens: diagnostics.llm_total_tokens,
        calls: diagnostics.llm_calls,
        estimated: diagnostics.llm_estimated,
    }
}

// ---------------------------------------------------------------------------
// 资源字段派生
// ---------------------------------------------------------------------------

/// 可下载资源的校验状态 → 卡片 note 文案。
fn verification_note(status: rdm_resolve::VerificationStatus) -> String {
    match status {
        rdm_resolve::VerificationStatus::Verified => "已通过本地校验".to_owned(),
        rdm_resolve::VerificationStatus::HeadOnlyVerified => {
            "仅 HEAD 校验（一次性/签名链接）".to_owned()
        }
        other => format!("校验状态：{other:?}"),
    }
}

/// `<state_dir>/resolve` 的 resolver 结果只给出 `rdm_ai::classify::category_for`
/// （中文分类目录名，如「视频」）；GUI 的 `resource_kind_label` 需要英文 kind
/// （`video`/`audio`/…）。这里把分类名映射回英文 kind，保持报告结构与 UI 文案不变。
fn kind_from_category(category: &str) -> &'static str {
    match category {
        "视频" => "video",
        "音频" => "audio",
        "图片" => "image",
        "压缩包" => "archive",
        "文档" => "document",
        "程序安装包" => "program",
        _ => "other",
    }
}

/// URL 末段作为文件名；空段回退 `download.bin`（与 `rdm-gui::model::derive_file_name`
/// 的兜底一致）。
fn file_name_from_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let path = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let segment = path.rsplit('/').next().unwrap_or_default().trim();
    if segment.is_empty() {
        "download.bin".to_owned()
    } else {
        segment.to_owned()
    }
}

/// 截断到最多 `max_chars` 个字符（按字符，不切断中文/emoji）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// P4b-b2: IPC/native 页面链接快照 → resolver `PageSnapshot`
// ---------------------------------------------------------------------------

/// 把 native/IPC 传来的页面链接快照转换成 resolver 的 [`rdm_resolve::PageSnapshot`]。
///
/// 契约（`docs/architecture.md` §11.12 P4b-b2）：`page_url` 必须存在且是 http(s)，
/// 其 host 必须与本次解析 `request_url` 的 host 一致（小写/去尾点归一）；否则
/// 返回 `None`（快照被忽略，解析照常进行）。links/title 的过滤、去重与上限复用
/// resolver 的 [`rdm_resolve::PageSnapshot::normalized`]，与 P4b-b1 完全一致。
///
/// 无 `page_url` 但有 links → `None`（无归属快照）；快照只在本机内存链路传递，
/// 不落盘、不进日志/解析缓存。
pub(crate) fn page_snapshot_from_ipc(
    request_url: &str,
    page_url: Option<&str>,
    page_title: Option<&str>,
    page_links: &[String],
) -> Option<rdm_resolve::PageSnapshot> {
    let page_url = page_url.map(str::trim).filter(|value| !value.is_empty())?;
    let request = url::Url::parse(request_url.trim()).ok()?;
    if !matches!(request.scheme(), "http" | "https") {
        return None;
    }
    let snapshot = rdm_resolve::PageSnapshot {
        url: page_url.to_owned(),
        title: page_title.map(str::to_owned),
        links: page_links.to_vec(),
    }
    .normalized();
    snapshot.is_valid_for(&request).then_some(snapshot)
}

// ---------------------------------------------------------------------------
// P3b: 方法健康度提示（`docs/architecture.md` §8.5 v0.16）
// ---------------------------------------------------------------------------

/// 健康度提示的最大长度（字符，UTF-8 安全截断）。
const MAX_METHOD_HEALTH_NOTE_CHARS: usize = 240;
/// 提示里嵌入的 step id 最大长度（字符）。
const MAX_HEALTH_STEP_CHARS: usize = 40;

/// 为 `degraded`/`broken` 的本地方法生成一条中文提示（`None` = 不提示）。
///
/// 契约（§8.5「方法健康度提示」）：提示只含方法 id@版本、状态、连续结构性失败次数、
/// 最近失败的 kind/step（不写 `run_id`、URL、cookie/header 值），并给出
/// `rdm recipe rollback`/导入新版本/`--mode explore` 建议；总长 ≤240 字符。
pub(crate) fn method_health_note(
    id: &str,
    version: u32,
    health: &rdm_resolve::RecipeHealth,
) -> Option<String> {
    let state = match health.status {
        rdm_resolve::MethodStatus::Degraded => "已降级",
        rdm_resolve::MethodStatus::Broken => "已失效",
        rdm_resolve::MethodStatus::Ok | rdm_resolve::MethodStatus::Untested => return None,
    };

    let mut note = format!(
        "本地方法 {id}@v{version} {state}（连续结构性失败 {} 次",
        health.consecutive_failures
    );
    if let Some(failure) = &health.last_failure {
        note.push_str(&format!(
            "，最近失败: {}",
            health_failure_kind_token(failure.kind)
        ));
        if let Some(step) = failure
            .step_id
            .as_deref()
            .map(str::trim)
            .filter(|step| !step.is_empty())
        {
            note.push('@');
            note.push_str(&truncate_chars(step, MAX_HEALTH_STEP_CHARS));
        }
    }
    note.push_str("）；可尝试 rdm recipe rollback、导入新版本或 --mode explore");

    // 防御：方法 id/step 来自本地方法库，但展示字段仍不应带控制字符。
    let note: String = note
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    Some(truncate_chars(&note, MAX_METHOD_HEALTH_NOTE_CHARS))
}

/// `ResolveErrorKind` → 稳定 snake_case token（serde 的 wire 表示）。
fn health_failure_kind_token(kind: rdm_resolve::ResolveErrorKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_resolve::{
        Diagnostics, ResolveOutcome, ResourceItem, ResourceManifest, RunStatus, SourceRef,
        Verification, VerificationStatus, SCHEMA_VERSION,
    };

    const REQUEST_URL: &str = "https://example.com/files/demo.bin";

    fn item(
        url: &str,
        name: Option<&str>,
        kind: Option<&str>,
        status: VerificationStatus,
        content_type: Option<&str>,
        size_bytes: Option<u64>,
    ) -> ResourceItem {
        ResourceItem {
            resource_id: format!("res_{url}"),
            url: url.to_owned(),
            final_url: Some(url.to_owned()),
            name: name.map(str::to_owned),
            kind: kind.map(str::to_owned),
            content_type: content_type.map(str::to_owned),
            size_bytes,
            supports_ranges: Some(true),
            etag: None,
            last_modified: None,
            expected_sha256: None,
            credentials: Default::default(),
            transfer: Default::default(),
            verification: Verification {
                status,
                final_url: Some(url.to_owned()),
                content_type: content_type.map(str::to_owned),
                size_bytes,
                ..Verification::default()
            },
            evidence: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn outcome(
        status: RunStatus,
        resources: Vec<ResourceItem>,
        notes: Vec<&str>,
        llm_calls: u32,
        recipe_id: Option<&str>,
    ) -> ResolveOutcome {
        ResolveOutcome {
            run_id: "run_test".to_owned(),
            status,
            manifest: Some(ResourceManifest {
                schema_version: SCHEMA_VERSION.to_owned(),
                manifest_id: "mf_test".to_owned(),
                source: SourceRef {
                    input_url: REQUEST_URL.to_owned(),
                    final_url: Some(REQUEST_URL.to_owned()),
                    title: Some("页面标题".to_owned()),
                    recipe_id: recipe_id.map(str::to_owned),
                },
                resources,
                notes: notes.into_iter().map(str::to_owned).collect(),
                created_ms: 1_700_000_000_000,
            }),
            recipe: None,
            diagnostics: Diagnostics {
                summary: "builtin direct-file verification: 1 resource(s)".to_owned(),
                llm_calls,
                ..Diagnostics::default()
            },
        }
    }

    fn direct_outcome() -> ResolveOutcome {
        outcome(
            RunStatus::Succeeded,
            vec![item(
                REQUEST_URL,
                Some("demo.bin"),
                None,
                VerificationStatus::Verified,
                Some("application/octet-stream"),
                Some(96 * 1024),
            )],
            vec!["manifest note"],
            0,
            None,
        )
    }

    #[test]
    fn maps_extensionless_title_to_mp4_using_content_type() {
        let outcome = outcome(
            RunStatus::Succeeded,
            vec![item(
                "https://cdn.example.com/upgcxcode/05/11/42924311105/stream?e=sig",
                Some("饿急眼了煮两包泡面 (720P)"),
                Some("video"),
                VerificationStatus::Verified,
                Some("video/mp4"),
                Some(45_932_387),
            )],
            vec![],
            1,
            None,
        );
        let report =
            report_from_outcome(REQUEST_URL, None, Some("mock"), &outcome).expect("must map");
        assert_eq!(report.resources[0].name, "饿急眼了煮两包泡面 (720P).mp4");
        assert_eq!(report.resources[0].kind, "video");
    }

    #[test]
    fn maps_a_direct_file_report() {
        let report =
            report_from_outcome(REQUEST_URL, Some("下载页"), Some("mock"), &direct_outcome())
                .expect("direct file must map");

        assert_eq!(report.source_url, REQUEST_URL);
        assert_eq!(report.source_kind, "direct_file");
        assert_eq!(report.title.as_deref(), Some("下载页"));
        assert!(!report.ai_used);
        assert!(report.model.is_none());
        assert!(!report.cache_hit);
        assert!(report.failure_kind.is_none());
        assert!(report.created_ms > 0);
        assert_eq!(report.resources.len(), 1);
        let resource = &report.resources[0];
        assert_eq!(resource.url, REQUEST_URL);
        assert_eq!(resource.name, "demo.bin");
        assert_eq!(resource.kind, "other", "octet-stream has no kind hint");
        assert_eq!(resource.size_bytes, Some(96 * 1024));
        assert_eq!(
            resource.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(resource
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("校验"));
        // manifest note first, diagnostics summary last; both short.
        assert_eq!(report.notes.len(), 2, "{:?}", report.notes);
        assert_eq!(report.notes[0], "manifest note");
        assert!(report.notes[1].contains("builtin direct-file verification"));
    }

    #[test]
    fn derives_name_and_kind_when_manifest_omits_them() {
        let mut outcome = direct_outcome();
        let manifest = outcome.manifest.as_mut().unwrap();
        manifest.resources[0].name = None;
        manifest.resources[0].kind = None;
        manifest.source.final_url = Some("https://cdn.example.com/media/clip.mp4".to_owned());
        manifest.resources[0].url = "https://cdn.example.com/media/clip.mp4".to_owned();
        manifest.resources[0].verification.final_url =
            Some("https://cdn.example.com/media/clip.mp4".to_owned());

        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        // Not the input URL anymore → html_page.
        assert_eq!(report.source_kind, "html_page");
        let resource = &report.resources[0];
        assert_eq!(resource.name, "clip.mp4");
        assert_eq!(resource.kind, "video", "derived from the .mp4 extension");
    }

    #[test]
    fn derives_kind_from_content_type_when_the_name_has_no_hint() {
        let mut outcome = direct_outcome();
        let manifest = outcome.manifest.as_mut().unwrap();
        manifest.resources[0].name = Some("download".to_owned());
        manifest.resources[0].kind = None;
        manifest.resources[0].content_type = Some("audio/flac".to_owned());
        manifest.resources[0].verification.content_type = Some("audio/flac".to_owned());

        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        assert_eq!(report.resources[0].kind, "audio");
    }

    #[test]
    fn explicit_resource_kind_wins_over_derivation() {
        let mut outcome = direct_outcome();
        outcome.manifest.as_mut().unwrap().resources[0].kind = Some("program".to_owned());
        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        assert_eq!(report.resources[0].kind, "program");
    }

    #[test]
    fn llm_use_marks_html_page_and_model() {
        let mut outcome = outcome(
            RunStatus::Succeeded,
            vec![item(
                "https://cdn.example.com/files/demo.bin",
                Some("demo.bin"),
                None,
                VerificationStatus::Verified,
                None,
                None,
            )],
            Vec::new(),
            3,
            None,
        );
        outcome.manifest.as_mut().unwrap().source.final_url = None;
        let report = report_from_outcome(REQUEST_URL, None, Some("mock-model"), &outcome)
            .expect("explored manifest must map");
        assert_eq!(report.source_kind, "html_page");
        assert!(report.ai_used);
        assert_eq!(report.model.as_deref(), Some("mock-model"));
    }

    #[test]
    fn model_is_dropped_when_the_model_was_not_called() {
        let report = report_from_outcome(REQUEST_URL, None, Some("mock-model"), &direct_outcome())
            .expect("report");
        assert!(report.model.is_none());
    }

    #[test]
    fn recipe_results_are_html_page_even_without_llm_calls() {
        let mut outcome = direct_outcome();
        outcome.manifest.as_mut().unwrap().source.recipe_id = Some("example-file".to_owned());
        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        assert_eq!(report.source_kind, "html_page");
    }

    #[test]
    fn multiple_resources_are_html_page() {
        let mut outcome = direct_outcome();
        let manifest = outcome.manifest.as_mut().unwrap();
        manifest.resources.push(item(
            "https://cdn.example.com/files/second.zip",
            Some("second.zip"),
            None,
            VerificationStatus::HeadOnlyVerified,
            Some("application/zip"),
            Some(10),
        ));
        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        assert_eq!(report.source_kind, "html_page");
        assert_eq!(report.resources.len(), 2);
    }

    #[test]
    fn non_downloadable_resources_do_not_map() {
        let mut outcome = outcome(
            RunStatus::Partial,
            vec![item(
                REQUEST_URL,
                Some("page.html"),
                None,
                VerificationStatus::HtmlPage,
                Some("text/html"),
                None,
            )],
            Vec::new(),
            0,
            None,
        );
        assert!(report_from_outcome(REQUEST_URL, None, None, &outcome).is_none());

        outcome.manifest = None;
        assert!(report_from_outcome(REQUEST_URL, None, None, &outcome).is_none());
    }

    #[test]
    fn head_only_resources_are_mapped() {
        let mut outcome = direct_outcome();
        let manifest = outcome.manifest.as_mut().unwrap();
        manifest.resources[0].verification.status = VerificationStatus::HeadOnlyVerified;
        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        let note = report.resources[0].note.clone().unwrap_or_default();
        assert!(note.contains("HEAD"), "{note}");
    }

    #[test]
    fn notes_are_bounded_to_eight_and_the_summary_is_truncated() {
        let long_summary = "摘".repeat(2000);
        let mut outcome = outcome(
            RunStatus::Succeeded,
            vec![item(
                REQUEST_URL,
                Some("demo.bin"),
                None,
                VerificationStatus::Verified,
                None,
                None,
            )],
            vec!["n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "", "n10"],
            0,
            None,
        );
        outcome.diagnostics = Diagnostics {
            summary: long_summary,
            llm_calls: 0,
            ..Diagnostics::default()
        };
        let report = report_from_outcome(REQUEST_URL, None, None, &outcome).expect("report");
        assert!(report.notes.len() <= MAX_NOTES, "{:?}", report.notes.len());
        assert_eq!(
            report.notes.len(),
            MAX_MANIFEST_NOTES + 1,
            "6 manifest notes + summary (empty notes are dropped, extras truncated)"
        );
        assert_eq!(report.notes[0], "n1");
        assert_eq!(report.notes[MAX_MANIFEST_NOTES - 1], "n6");
        assert!(
            report.notes[MAX_MANIFEST_NOTES].chars().count() <= SUMMARY_MAX_CHARS,
            "summary must be truncated"
        );
        assert!(report.notes[MAX_MANIFEST_NOTES].ends_with('…'));
    }

    #[test]
    fn empty_manifest_view_yields_no_report() {
        // A manifest whose only resources were filtered out (defensive).
        let mut outcome = direct_outcome();
        outcome.manifest.as_mut().unwrap().resources.clear();
        assert!(report_from_outcome(REQUEST_URL, None, None, &outcome).is_none());
    }

    // ------------------------------------------------------------------
    // P4b-b2: IPC page snapshot conversion
    // ------------------------------------------------------------------

    const PAGE_URL: &str = "https://example.com/page?x=1";

    #[test]
    fn page_snapshot_converts_a_matching_snapshot() {
        let links = vec![
            "https://cdn.example.com/a.bin".to_owned(),
            "https://cdn.example.com/a.bin".to_owned(),
            "https://cdn.example.com/b.zip".to_owned(),
        ];
        let snapshot = page_snapshot_from_ipc(
            "https://Example.com./page",
            Some("  https://EXAMPLE.com./page?x=1  "),
            Some("  Demo\tPage  "),
            &links,
        )
        .expect("matching host must convert");

        // Normalized page URL/host; the resolver re-checks the host per run
        // (it strips the trailing dot for comparison but keeps it in the URL).
        assert_eq!(snapshot.url, "https://example.com./page?x=1");
        assert_eq!(snapshot.title.as_deref(), Some("Demo\tPage"));
        assert_eq!(
            snapshot.links,
            vec![
                "https://cdn.example.com/a.bin".to_string(),
                "https://cdn.example.com/b.zip".to_string()
            ],
            "links are deduplicated in order"
        );
        assert!(snapshot.is_valid_for(&url::Url::parse(PAGE_URL).unwrap()));
    }

    #[test]
    fn page_snapshot_rejects_a_foreign_or_invalid_page_url() {
        for page_url in [
            "https://evil.example/page",    // different host
            "https://sub.example.com/page", // subdomain is a different host
            "http://example.com/page",      // scheme mismatch is still the same host
            "file:///C:/Windows/win.ini",   // non-http(s)
            "javascript:alert(1)",
            "not a url",
        ] {
            let snapshot = page_snapshot_from_ipc(
                PAGE_URL,
                Some(page_url),
                Some("title"),
                &["https://cdn.example.com/a.bin".to_owned()],
            );
            // http(s)://example.com with another scheme still belongs to the same
            // host, so only the truly foreign/invalid URLs are rejected.
            if page_url == "http://example.com/page" {
                continue;
            }
            assert!(snapshot.is_none(), "{page_url} must be rejected");
        }
    }

    #[test]
    fn page_snapshot_requires_a_page_url() {
        let links = vec!["https://cdn.example.com/a.bin".to_owned()];
        assert!(page_snapshot_from_ipc(PAGE_URL, None, Some("t"), &links).is_none());
        assert!(page_snapshot_from_ipc(PAGE_URL, Some("   "), Some("t"), &links).is_none());

        // A page URL without links is still a useful snapshot (login page with
        // no visible download links yet).
        let empty = page_snapshot_from_ipc(PAGE_URL, Some(PAGE_URL), None, &[]);
        assert_eq!(empty.expect("empty snapshot is valid").links.len(), 0);
    }

    #[test]
    fn page_snapshot_filters_and_bounds_links() {
        let mut links: Vec<String> = vec![
            "https://cdn.example.com/ok.bin".to_owned(),
            "/relative.bin".to_owned(),
            "javascript:alert(1)".to_owned(),
            "https://cdn.example.com/ok.bin".to_owned(),
            "   ".to_owned(),
            format!(
                "https://cdn.example.com/{}",
                "x".repeat(rdm_resolve::MAX_SNAPSHOT_LINK)
            ),
        ];
        for index in 0..(rdm_resolve::MAX_SNAPSHOT_LINKS + 5) {
            links.push(format!("https://cdn.example.com/{index}.bin"));
        }

        let snapshot = page_snapshot_from_ipc(PAGE_URL, Some(PAGE_URL), None, &links)
            .expect("snapshot with matching host");
        assert_eq!(snapshot.links.len(), rdm_resolve::MAX_SNAPSHOT_LINKS);
        assert_eq!(snapshot.links[0], "https://cdn.example.com/ok.bin");
        assert!(snapshot
            .links
            .iter()
            .all(|link| link.starts_with("https://") && !link.contains("relative")));
    }

    #[test]
    fn page_snapshot_rejects_invalid_request_urls() {
        for request_url in ["not a url", "file:///tmp/x", "ftp://example.com/x", ""] {
            assert!(
                page_snapshot_from_ipc(
                    request_url,
                    Some(PAGE_URL),
                    None,
                    &["https://cdn.example.com/a.bin".to_owned()]
                )
                .is_none(),
                "{request_url:?} must be rejected"
            );
        }
    }

    #[test]
    fn page_snapshot_title_is_truncated_by_the_resolver_normalizer() {
        let long_title = "标".repeat(rdm_resolve::MAX_SNAPSHOT_TITLE * 2);
        let snapshot = page_snapshot_from_ipc(PAGE_URL, Some(PAGE_URL), Some(&long_title), &[])
            .expect("snapshot");
        assert_eq!(
            snapshot
                .title
                .as_deref()
                .map(str::chars)
                .map(Iterator::count),
            Some(rdm_resolve::MAX_SNAPSHOT_TITLE)
        );
    }

    // ------------------------------------------------------------------
    // P3b: method health note (`docs/architecture.md` §8.5 v0.16)
    // ------------------------------------------------------------------

    fn health(
        status: rdm_resolve::MethodStatus,
        consecutive_failures: u32,
        last_failure: Option<(&str, rdm_resolve::ResolveErrorKind, Option<&str>)>,
    ) -> rdm_resolve::RecipeHealth {
        rdm_resolve::RecipeHealth {
            status,
            last_ok_ms: None,
            last_failure: last_failure.map(|(run_id, kind, step_id)| rdm_resolve::FailureRef {
                run_id: run_id.to_owned(),
                kind,
                step_id: step_id.map(str::to_owned),
                observed_ms: 1,
            }),
            consecutive_failures,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn method_health_note_reports_degraded_methods() {
        let health = health(
            rdm_resolve::MethodStatus::Degraded,
            2,
            Some((
                "run_secret_id",
                rdm_resolve::ResolveErrorKind::RecipeStale,
                Some("url"),
            )),
        );
        let note = method_health_note("example.com-file", 3, &health).expect("degraded note");
        assert!(note.contains("example.com-file"));
        assert!(note.contains("@v3"));
        assert!(note.contains("已降级"));
        assert!(note.contains("连续结构性失败 2 次"));
        assert!(note.contains("recipe_stale@url"), "{note}");
        assert!(note.contains("rdm recipe rollback"));
        assert!(note.contains("--mode explore"));
        // 不写 run_id，也不含 URL/secret。
        assert!(!note.contains("run_secret_id"), "{note}");
        assert!(!note.contains("https://"), "{note}");
    }

    #[test]
    fn method_health_note_reports_broken_methods() {
        let health = health(
            rdm_resolve::MethodStatus::Broken,
            3,
            Some(("run_1", rdm_resolve::ResolveErrorKind::SelectorMiss, None)),
        );
        let note = method_health_note("demo-method", 2, &health).expect("broken note");
        assert!(note.contains("demo-method@v2"));
        assert!(note.contains("已失效"));
        assert!(note.contains("连续结构性失败 3 次"));
        assert!(note.contains("selector_miss"));
        assert!(note.contains("rdm recipe rollback"));
    }

    #[test]
    fn method_health_note_is_silent_for_ok_and_untested() {
        assert!(
            method_health_note("m", 1, &health(rdm_resolve::MethodStatus::Ok, 0, None)).is_none()
        );
        assert!(method_health_note(
            "m",
            1,
            &health(rdm_resolve::MethodStatus::Untested, 0, None)
        )
        .is_none());
        // Even with a recorded last failure, ok/untested never warn.
        assert!(method_health_note(
            "m",
            1,
            &health(
                rdm_resolve::MethodStatus::Ok,
                0,
                Some(("run_1", rdm_resolve::ResolveErrorKind::AuthRequired, None)),
            )
        )
        .is_none());
    }

    #[test]
    fn method_health_note_handles_missing_failure_details() {
        let health = health(rdm_resolve::MethodStatus::Degraded, 2, None);
        let note = method_health_note("m", 1, &health).expect("degraded note");
        assert!(note.contains("连续结构性失败 2 次"));
        assert!(!note.contains("最近失败"), "{note}");
        assert!(!note.contains("run_id"), "{note}");
    }

    #[test]
    fn method_health_note_is_truncated_to_240_chars() {
        let long_id = "m".repeat(400);
        let long_step = "s".repeat(400);
        let health = health(
            rdm_resolve::MethodStatus::Broken,
            3,
            Some((
                "run_1",
                rdm_resolve::ResolveErrorKind::RecipeStale,
                Some(&long_step),
            )),
        );
        let note = method_health_note(&long_id, 1, &health).expect("note");
        assert!(note.chars().count() <= MAX_METHOD_HEALTH_NOTE_CHARS);
        assert!(note.ends_with('…'), "{note}");
    }

    #[test]
    fn method_health_note_strips_control_characters() {
        let health = health(
            rdm_resolve::MethodStatus::Degraded,
            2,
            Some((
                "run_1",
                rdm_resolve::ResolveErrorKind::RecipeStale,
                Some("ur\nl\u{7}"),
            )),
        );
        let note = method_health_note("good-method", 1, &health).expect("note");
        assert!(!note.chars().any(char::is_control), "{note}");
        assert!(note.contains("recipe_stale@url"), "{note}");
    }
}
