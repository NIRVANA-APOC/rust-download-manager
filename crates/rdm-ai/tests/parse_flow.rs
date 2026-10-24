//! End-to-end `parse_url` tests against the in-crate mock HTTP server.
//!
//! Covers: direct-file short circuit (HEAD and GET fallback), HTML heuristics,
//! LLM success/fenced JSON/failure fallback, cache hit/expiry/disk roundtrip,
//! credential handling (used for fetch, never persisted) and URL validation.

mod support;

use rdm_ai::{
    parse_url, AiConfig, ParseCache, ParseOptions, ParseReport, ParseRequest, ParsedResource,
};
use support::{ai_config, local_config, MockServer, Reply, TempDir};

const PAGE_HTML: &str = r##"<html><head><title>Demo Page</title></head><body>
  <a href="/media/movie.mp4">movie</a>
  <a href="/media/song.mp3">song</a>
  <a href="/other/archive.zip">zip</a>
  <a href="javascript:void(0)">js link</a>
  <a href="mailto:nobody@example.com">mail</a>
</body></html>"##;

const LLM_REPLY: &str = r#"{"resources":[
  {"url":"https://cdn.example.com/ai-pick.mp4","name":"ai-pick.mp4","kind":"video",
   "size_bytes":111,"content_type":"video/mp4","note":"推荐"}
],"notes":["该站需要 Referer"]}"#;

fn request(url: &str) -> ParseRequest {
    ParseRequest {
        url: url.to_owned(),
        ..ParseRequest::default()
    }
}

fn report_fixture(source_url: &str, created_ms: u64) -> ParseReport {
    ParseReport {
        source_url: source_url.to_owned(),
        source_kind: "html_page".to_owned(),
        title: Some("Cached Title".to_owned()),
        resources: vec![ParsedResource {
            url: "https://cdn.example.com/cached.mp4".to_owned(),
            name: "cached.mp4".to_owned(),
            kind: "video".to_owned(),
            size_bytes: None,
            content_type: None,
            note: None,
        }],
        notes: vec!["cached note".to_owned()],
        ai_used: false,
        model: None,
        usage: rdm_ai::TokenUsage::default(),
        created_ms,
        cache_hit: false,
        failure_kind: None,
    }
}

#[tokio::test]
async fn direct_file_via_head_short_circuits_llm() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/files/movie.bin") => Reply::status(200, "")
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Disposition", "attachment; filename=\"movie.mp4\"")
                    .content_length(12345),
                ("GET", "/files/movie.bin") => Reply::status(200, "")
                    .header("Content-Type", "application/octet-stream")
                    .body("0123456789"),
                ("POST", _) => Reply::chat(LLM_REPLY),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/files/movie.bin")),
        &ai_config(&server),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert_eq!(report.source_kind, "direct_file");
    assert!(!report.ai_used);
    assert_eq!(report.model, None);
    assert_eq!(report.resources.len(), 1);
    let resource = &report.resources[0];
    assert_eq!(resource.url, server.url("/files/movie.bin"));
    assert_eq!(resource.name, "movie.mp4");
    assert_eq!(resource.kind, "video");
    assert_eq!(resource.size_bytes, Some(12345));
    assert_eq!(server.count_method("GET"), 0);
    assert_eq!(server.count_method("POST"), 0);
    assert!(cache.get(&report.source_url).is_some());
}

#[tokio::test]
async fn direct_file_via_get_when_head_is_rejected() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/download/clip.mp4") => Reply::status(405, "HEAD not allowed"),
                ("GET", "/download/clip.mp4") => Reply::status(200, "")
                    .header("Content-Type", "video/mp4")
                    .body("0123456789"),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/download/clip.mp4")),
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert_eq!(report.source_kind, "direct_file");
    assert_eq!(server.count_method("HEAD"), 1);
    assert_eq!(server.count_method("GET"), 1);
    let resource = &report.resources[0];
    assert_eq!(resource.name, "clip.mp4");
    assert_eq!(resource.kind, "video");
    assert_eq!(resource.size_bytes, Some(10));
}

#[tokio::test]
async fn html_page_without_ai_uses_heuristics() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/page")),
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert_eq!(report.source_kind, "html_page");
    assert_eq!(report.title.as_deref(), Some("Demo Page"));
    assert!(!report.ai_used);
    assert_eq!(report.model, None);
    assert_eq!(server.count_method("POST"), 0);
    assert!(
        report
            .resources
            .iter()
            .any(|resource| resource.url.ends_with("/media/movie.mp4") && resource.kind == "video"),
        "{:?}",
        report.resources
    );
    assert!(report
        .resources
        .iter()
        .any(|resource| resource.kind == "audio"));
    assert!(report
        .resources
        .iter()
        .any(|resource| resource.kind == "archive"));
    assert!(
        report.notes.iter().any(|note| note.contains("启发式")),
        "{:?}",
        report.notes
    );
    // Every fetched link was filtered (`javascript:` / `mailto:` never appear).
    assert!(report
        .resources
        .iter()
        .all(|resource| { resource.url.starts_with(&server.url("")) }));
}

#[tokio::test]
async fn html_page_with_ai_uses_llm_resources() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::chat(LLM_REPLY),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert_eq!(report.source_kind, "html_page");
    assert!(report.ai_used);
    assert_eq!(report.model.as_deref(), Some("mock-model"));
    assert_eq!(report.resources.len(), 1);
    assert_eq!(
        report.resources[0].url,
        "https://cdn.example.com/ai-pick.mp4"
    );
    assert_eq!(report.resources[0].kind, "video");
    assert_eq!(report.resources[0].size_bytes, Some(111));
    assert_eq!(report.notes, vec!["该站需要 Referer".to_owned()]);
    // The mock provider sends no `usage` object, so the report carries a
    // clearly-marked estimate for the one model call — never a fake exact 0.
    assert!(
        report.usage.calls >= 1,
        "successful LLM parse must report its model call: {:?}",
        report.usage
    );
    assert!(
        report.usage.estimated,
        "missing provider usage must be marked estimated: {:?}",
        report.usage
    );
    assert!(
        report.usage.total_tokens > 0,
        "estimated usage must be non-zero: {:?}",
        report.usage
    );

    let post = server
        .requests()
        .into_iter()
        .find(|request| request.method == "POST")
        .expect("LLM request");
    assert!(post.body.contains("/media/movie.mp4"), "{}", post.body);
    assert!(post.body.contains("Demo Page"), "{}", post.body);

    // The result was written back into the cache.
    let cached = cache.get(&report.source_url).expect("cache entry");
    assert_eq!(cached.resources.len(), 1);
    assert_eq!(cached.model.as_deref(), Some("mock-model"));
    assert!(!cached.instructions.is_empty());
}

#[tokio::test]
async fn html_page_accepts_fenced_llm_json() {
    let fenced = format!("```json\n{LLM_REPLY}\n```");
    let server =
        MockServer::start(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::chat(&fenced),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;
    assert!(report.ai_used);
    assert_eq!(
        report.resources[0].url,
        "https://cdn.example.com/ai-pick.mp4"
    );
}

#[tokio::test]
async fn html_page_falls_back_to_heuristics_when_llm_fails() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::status(500, "llm down"),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert!(!report.ai_used);
    assert_eq!(report.model, None);
    assert!(report
        .resources
        .iter()
        .any(|resource| resource.url.ends_with("/media/movie.mp4")));
    assert!(
        report.notes.iter().any(|note| note.contains("AI 调用失败")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn fresh_cache_hit_skips_http_and_llm() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::chat(LLM_REPLY),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let options = ParseOptions::default();
    let first = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &options,
    )
    .await;
    let requests_after_first = server.request_count();
    assert!(requests_after_first >= 3);

    let second = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &options,
    )
    .await;

    assert!(second.cache_hit);
    assert_eq!(second.resources, first.resources);
    assert_eq!(second.source_kind, first.source_kind);
    assert_eq!(server.request_count(), requests_after_first);
}

#[tokio::test]
async fn expired_cache_entry_is_refreshed() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    cache.put(&report_fixture(&server.url("/page"), 0), "seed");
    let options = ParseOptions {
        use_cache: true,
        max_age_ms: 1,
        max_resources: 40,
    };
    let report = parse_url(
        &request(&server.url("/page")),
        &local_config(),
        &mut cache,
        &options,
    )
    .await;

    assert!(!report.cache_hit);
    assert_eq!(report.title.as_deref(), Some("Demo Page"));
    assert!(server.request_count() >= 2);
}

#[tokio::test]
async fn cookie_is_used_for_fetch_but_never_persisted() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let temp = TempDir::new("cookie");
    let mut cache = ParseCache::default();
    let parse_request = ParseRequest {
        url: server.url("/page"),
        cookie: Some("session=top-secret-cookie".to_owned()),
        referer: Some(server.url("/ref")).to_owned(),
        user_agent: Some("RDM-Test/1.0".to_owned()),
        ..ParseRequest::default()
    };
    let report = parse_url(
        &parse_request,
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    // The credential was sent while fetching.
    let head = server
        .requests()
        .into_iter()
        .find(|request| request.method == "HEAD")
        .expect("HEAD request");
    assert_eq!(head.header("cookie"), Some("session=top-secret-cookie"));
    assert_eq!(head.header("referer"), Some(server.url("/ref").as_str()));

    // … and never written into the report JSON or the on-disk cache.
    let report_json = serde_json::to_string(&report).unwrap();
    assert!(!report_json.contains("top-secret-cookie"), "{report_json}");
    cache.save(temp.path()).unwrap();
    let cache_text = std::fs::read_to_string(ParseCache::cache_path(temp.path())).unwrap();
    assert!(!cache_text.contains("top-secret-cookie"), "{cache_text}");
    assert!(!cache_text.contains("RDM-Test/1.0"), "{cache_text}");
}

#[tokio::test]
async fn invalid_url_degrades_without_network() {
    let mut cache = ParseCache::default();
    let report = parse_url(
        &request("javascript:alert(1)"),
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;
    assert_eq!(report.source_kind, "unknown");
    assert!(report.resources.is_empty());
    assert!(!report.ai_used);
    assert!(
        report.notes.iter().any(|note| note.contains("http(s)")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn max_resources_is_enforced_for_llm_results() {
    let resources: Vec<serde_json::Value> = (0..8)
        .map(|index| {
            serde_json::json!({
                "url": format!("https://cdn.example.com/file-{index}.mp4"),
                "name": format!("file-{index}.mp4"),
                "kind": "video"
            })
        })
        .collect();
    let reply = serde_json::json!({"resources": resources, "notes": []}).to_string();
    let server =
        MockServer::start(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::chat(&reply),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;

    let mut cache = ParseCache::default();
    let options = ParseOptions {
        max_resources: 3,
        ..ParseOptions::default()
    };
    let report = parse_url(
        &request(&server.url("/page")),
        &ai_config(&server),
        &mut cache,
        &options,
    )
    .await;
    assert_eq!(report.resources.len(), 3);
}

#[tokio::test]
async fn fetch_failure_degrades_to_url_guess() {
    let server = MockServer::start(|_| Reply::status(404, "gone")).await;
    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/files/broken.mp4")),
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;

    assert_eq!(report.source_kind, "unknown");
    assert_eq!(report.resources.len(), 1);
    assert_eq!(report.resources[0].name, "broken.mp4");
    assert_eq!(report.resources[0].kind, "video");
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("抓取页面失败")));
}

#[tokio::test]
async fn cache_roundtrips_through_disk() {
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;
    let temp = TempDir::new("disk-cache");
    let parse_request = request(&server.url("/page"));
    let options = ParseOptions::default();

    let mut cache = ParseCache::default();
    let first = parse_url(&parse_request, &local_config(), &mut cache, &options).await;
    cache.save(temp.path()).unwrap();
    let requests_before = server.request_count();

    let mut loaded = ParseCache::load(temp.path());
    let second = parse_url(&parse_request, &local_config(), &mut loaded, &options).await;
    assert!(second.cache_hit);
    assert_eq!(second.resources, first.resources);
    assert_eq!(server.request_count(), requests_before);
}

#[tokio::test]
async fn ai_config_is_untouched_by_parsing() {
    // parse_url only borrows the config; the key is never echoed into reports.
    let server =
        MockServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("HEAD", "/page") | ("GET", "/page") => Reply::html(PAGE_HTML),
                ("POST", "/v1/chat/completions") => Reply::chat(LLM_REPLY),
                _ => Reply::status(404, "not found"),
            },
        )
        .await;
    let config: AiConfig = ai_config(&server);
    let key = config.api_key.clone();
    let mut cache = ParseCache::default();
    let report = parse_url(
        &request(&server.url("/page")),
        &config,
        &mut cache,
        &ParseOptions::default(),
    )
    .await;
    let rendered = serde_json::to_string(&report).unwrap() + &format!("{report:?}");
    assert!(!rendered.contains(&key), "{rendered}");
    assert!(!cache
        .get(&report.source_url)
        .unwrap()
        .instructions
        .contains(&key));
}

#[tokio::test]
async fn transient_failure_reports_kind_and_is_not_cached() {
    // Regression: a 5xx used to be cached for the full 7-day TTL, so one bad
    // minute hid the site until the entry expired.
    let server = MockServer::start(|_| Reply::status(500, "boom")).await;
    let temp = TempDir::new("failure-not-cached");
    let url = server.url("/page");
    let options = ParseOptions::default();

    let mut cache = ParseCache::default();
    let first = parse_url(&request(&url), &local_config(), &mut cache, &options).await;
    assert!(!first.cache_hit);
    assert_eq!(first.failure_kind.as_deref(), Some("server_error"));
    cache.save(temp.path()).unwrap();

    let requests_before = server.request_count();
    let mut reloaded = ParseCache::load(temp.path());
    let second = parse_url(&request(&url), &local_config(), &mut reloaded, &options).await;
    assert!(
        !second.cache_hit,
        "transient failures must not be served from cache"
    );
    assert!(
        server.request_count() > requests_before,
        "a transient failure must be retried on the network"
    );
}

#[tokio::test]
async fn permanent_failure_is_cached_but_marked() {
    // 404 is unlikely to change within minutes: cache it briefly, but keep the
    // failure visible so callers do not treat it as a successful parse.
    let server = MockServer::start(|_| Reply::status(404, "missing")).await;
    let temp = TempDir::new("not-found-cached");
    let url = server.url("/gone");
    let options = ParseOptions::default();

    let mut cache = ParseCache::default();
    let first = parse_url(&request(&url), &local_config(), &mut cache, &options).await;
    assert_eq!(first.failure_kind.as_deref(), Some("not_found"));
    cache.save(temp.path()).unwrap();
    drop(cache);

    let mut reloaded = ParseCache::load(temp.path());
    let second = parse_url(&request(&url), &local_config(), &mut reloaded, &options).await;
    assert!(second.cache_hit);
    assert_eq!(second.failure_kind.as_deref(), Some("not_found"));
}

#[tokio::test]
async fn stale_degraded_entry_is_refetched() {
    let server = MockServer::start(|_| Reply::status(500, "boom")).await;
    let url = server.url("/page");
    let mut cache = ParseCache::default();
    cache.entries.insert(
        url.clone(),
        rdm_ai::CachedParse {
            url: url.clone(),
            source_kind: "unknown".to_owned(),
            title: None,
            resources: Vec::new(),
            notes: Vec::new(),
            // Epoch: far older than any degraded TTL.
            created_ms: 0,
            model: None,
            instructions: String::new(),
            failure_kind: Some("not_found".to_owned()),
        },
    );

    let report = parse_url(
        &request(&url),
        &local_config(),
        &mut cache,
        &ParseOptions::default(),
    )
    .await;
    assert!(
        !report.cache_hit,
        "an expired degraded entry must not be replayed as fresh"
    );
    assert!(server.request_count() > 0);
}
