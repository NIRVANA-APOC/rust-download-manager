/*!
 * lib/pure.js 的单元测试（Node 内置 test runner）。
 *
 *   node --test extension/firefox/test/
 *
 * 覆盖：文件名清理、扩展名过滤、URL 判定、Content-Disposition 解析、
 * 设置归一化、接管决策矩阵、Cookie 头拼接、日志脱敏等纯逻辑。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const pure = require("../lib/pure.js");

test("sanitizeFileName: 去掉目录与非法字符", () => {
  assert.equal(pure.sanitizeFileName(String.raw`..\..\evil.exe`), "evil.exe");
  assert.equal(pure.sanitizeFileName("/tmp/a<b>c.iso"), "a_b_c.iso");
  assert.equal(pure.sanitizeFileName("   "), "");
  assert.equal(pure.sanitizeFileName(".."), "");
  assert.equal(pure.sanitizeFileName("C:\\dir\\"), "dir");
  assert.equal(pure.sanitizeFileName("a\u0000b.bin"), "ab.bin");
  const longName = "汉".repeat(300);
  assert.ok(Array.from(pure.sanitizeFileName(longName)).length <= 180);
});

test("extensionOf: 只取最后一段小写扩展名", () => {
  assert.equal(pure.extensionOf("big.iso"), "iso");
  assert.equal(pure.extensionOf("ARCHIVE.TAR"), "tar");
  assert.equal(pure.extensionOf("noext"), "");
  assert.equal(pure.extensionOf(".hidden"), "");
  assert.equal(pure.extensionOf("trailing."), "");
  assert.equal(pure.extensionOf(String.raw`C:\tmp\file.zip`), "zip");
});

test("isHttpUrl: 只接受 http/https", () => {
  assert.equal(pure.isHttpUrl("https://example.com/a?b=1"), true);
  assert.equal(pure.isHttpUrl("http://127.0.0.1:8080/x"), true);
  for (const bad of [
    "file:///C:/Windows/win.ini",
    "data:text/plain,hi",
    "blob:https://example.com/1",
    "javascript:alert(1)",
    "ftp://example.com/x",
    "not a url",
    "",
    null,
  ]) {
    assert.equal(pure.isHttpUrl(bad), false, String(bad));
  }
});

test("fileNameFromUrl: URL 解码与安全清理", () => {
  assert.equal(
    pure.fileNameFromUrl("https://example.com/files/%E4%B8%AD%E6%96%87.zip?token=x"),
    "中文.zip"
  );
  assert.equal(pure.fileNameFromUrl("https://example.com/a/b.bin"), "b.bin");
  assert.equal(pure.fileNameFromUrl("https://example.com/"), "");
  assert.equal(pure.fileNameFromUrl("nonsense"), "");
});

test("parseContentDispositionFilename: RFC 5987 与普通 filename", () => {
  assert.equal(
    pure.parseContentDispositionFilename(
      "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.iso"
    ),
    "中文.iso"
  );
  assert.equal(
    pure.parseContentDispositionFilename('attachment; filename="my file (1).zip"'),
    "my file (1).zip"
  );
  assert.equal(
    pure.parseContentDispositionFilename("attachment; filename=plain.bin"),
    "plain.bin"
  );
  assert.equal(pure.parseContentDispositionFilename("attachment"), "");
  assert.equal(pure.parseContentDispositionFilename(""), "");
  assert.equal(pure.parseContentDispositionFilename(null), "");
});

test("normalizeSettings: 永远返回完整且被夹紧的设置（不再包含 connections）", () => {
  const settings = pure.normalizeSettings({
    enabled: "false",
    connections: 999,
    minSizeBytes: -5,
    speedLimitKiBps: -1,
    allowExtensions: [".ISO", "iso", "bad ext", 42],
    outputDir: "  D:\\Downloads  ",
    unknownKey: "ignored",
  });
  assert.equal(settings.enabled, false);
  // 连接数由 RDM 桌面端「设置 → 下载 → 默认连接数」统一决定：扩展设置不再持久化它。
  assert.equal("connections" in settings, false, "normalizeSettings 不得再输出 connections");
  assert.equal(settings.minSizeBytes, 0);
  assert.equal(settings.speedLimitKiBps, 0);
  assert.deepEqual(settings.allowExtensions, ["iso"]);
  assert.equal(settings.outputDir, "D:\\Downloads");
  assert.equal("unknownKey" in settings, false);
  // 空输入 → 默认值
  assert.deepEqual(pure.normalizeSettings(undefined), pure.DEFAULT_SETTINGS);
  assert.deepEqual(pure.normalizeSettings(null), pure.DEFAULT_SETTINGS);
});

test("parseExtensionList: 逗号/空格/中文标点分隔、去重", () => {
  assert.deepEqual(pure.parseExtensionList("iso, .ZIP 7z，rar；iso"), [
    "iso",
    "zip",
    "7z",
    "rar",
  ]);
  assert.deepEqual(pure.parseExtensionList(""), []);
  assert.deepEqual(pure.parseExtensionList("bad/entry !!!"), []);
  assert.equal(pure.formatExtensionList(["iso", "zip"]), "iso, zip");
});

test("shouldIntercept: 决策矩阵", () => {
  const settings = pure.normalizeSettings({});
  const base = { url: "https://example.com/big.iso", fileName: "big.iso", sizeBytes: 10 * 1024 * 1024 };

  assert.deepEqual(pure.shouldIntercept({ ...base, settings }), {
    intercept: true,
    reason: "ok",
  });
  assert.equal(
    pure.shouldIntercept({ ...base, settings: pure.normalizeSettings({ enabled: false }) }).reason,
    "disabled"
  );
  assert.equal(
    pure.shouldIntercept({ ...base, url: "file:///c:/x", settings }).reason,
    "non_http"
  );
  assert.equal(pure.shouldIntercept({ ...base, fallbackPending: true, settings }).reason, "fallback");
  assert.equal(
    pure.shouldIntercept({
      ...base,
      settings: pure.normalizeSettings({ blockExtensions: ["iso"] }),
    }).reason,
    "blocked_extension"
  );
  assert.equal(
    pure.shouldIntercept({
      ...base,
      settings: pure.normalizeSettings({ allowExtensions: ["zip"] }),
    }).reason,
    "not_in_allow_list"
  );
  assert.equal(
    pure.shouldIntercept({
      ...base,
      settings: pure.normalizeSettings({ allowExtensions: ["iso"] }),
    }).intercept,
    true
  );
  assert.equal(
    pure.shouldIntercept({
      ...base,
      sizeBytes: 1024,
      settings: pure.normalizeSettings({ minSizeBytes: 1024 * 1024 }),
    }).reason,
    "too_small"
  );
  // 大小未知时不因 minSize 拒绝
  assert.equal(
    pure.shouldIntercept({
      ...base,
      sizeBytes: -1,
      settings: pure.normalizeSettings({ minSizeBytes: 1024 * 1024 }),
    }).intercept,
    true
  );
  // 白名单 + 无文件名 → 不接管
  assert.equal(
    pure.shouldIntercept({
      ...base,
      fileName: "",
      settings: pure.normalizeSettings({ allowExtensions: ["iso"] }),
    }).reason,
    "not_in_allow_list"
  );
});

test("buildCookieHeader: 拼接、过滤控制字符、限量", () => {
  assert.equal(
    pure.buildCookieHeader([
      { name: "a", value: "1" },
      { name: "b", value: "2" },
    ]),
    "a=1; b=2"
  );
  assert.equal(
    pure.buildCookieHeader([
      { name: "ok", value: "1" },
      { name: "bad", value: "x\r\nInjected: 1" },
      { name: "", value: "skip" },
    ]),
    "ok=1"
  );
  const huge = Array.from({ length: 3000 }, (_, i) => ({ name: "n" + i, value: "v" }));
  const header = pure.buildCookieHeader(huge);
  assert.ok(header.length <= 16 * 1024, "cookie header must stay bounded");
  assert.ok(header.startsWith("n0=v"));
});

test("redactUrl: 去掉 query / fragment / 凭据", () => {
  assert.equal(
    pure.redactUrl("https://user:pass@example.com:8443/a/b.iso?sig=secret#frag"),
    "https://example.com:8443/a/b.iso"
  );
  assert.equal(pure.redactUrl("nonsense"), "");
});

test("formatAge / formatBytes / MB 换算", () => {
  const now = 1_700_000_000_000;
  assert.equal(pure.formatAge(now - 1000, now), "刚刚");
  assert.equal(pure.formatAge(now - 30_000, now), "30 秒前");
  assert.equal(pure.formatAge(now - 5 * 60_000, now), "5 分钟前");
  assert.equal(pure.formatAge(now - 3 * 3600_000, now), "3 小时前");
  assert.equal(pure.formatAge(now - 2 * 86400_000, now), "2 天前");
  assert.equal(pure.formatAge(0, now), "");

  assert.equal(pure.formatBytes(512), "512 B");
  assert.equal(pure.formatBytes(1536), "1.5 KiB");
  assert.equal(pure.formatBytes(10 * 1024 * 1024), "10 MiB");
  assert.equal(pure.formatBytes(-1), "未知");

  assert.equal(pure.parseMegabytesToBytes("1.5"), 1572864);
  assert.equal(pure.parseMegabytesToBytes("0"), 0);
  assert.equal(pure.parseMegabytesToBytes("abc"), 0);
  assert.equal(pure.formatMegabytesFromBytes(1572864), "1.5");
  assert.equal(pure.formatMegabytesFromBytes(0), "");
});

test("DEFAULT_SETTINGS 是冻结对象且字段完整", () => {
  assert.ok(Object.isFrozen(pure.DEFAULT_SETTINGS));
  for (const key of [
    "enabled",
    "minSizeBytes",
    "allowExtensions",
    "blockExtensions",
    "outputDir",
    "speedLimitKiBps",
    "autoStartGui",
    "forwardCookies",
    "contextMenu",
  ]) {
    assert.ok(key in pure.DEFAULT_SETTINGS, "missing default setting: " + key);
  }
  // 连接数是 GUI 的权威设置，扩展不再有默认值。
  assert.equal("connections" in pure.DEFAULT_SETTINGS, false);
  assert.equal(typeof pure.MIN_CONNECTIONS, "undefined");
  assert.equal(typeof pure.MAX_CONNECTIONS, "undefined");
});

// ---------------------------------------------------------------------------
// AI 解析（ai_parse）纯逻辑
// ---------------------------------------------------------------------------

test("sanitizeAiText: 去控制字符、折叠空白、限长", () => {
  assert.equal(pure.sanitizeAiText("  hello\u0007   world  ", 100), "hello world");
  assert.equal(
    pure.sanitizeAiText("Mozilla/5.0\nInjected: x", 100),
    "Mozilla/5.0 Injected: x"
  );
  assert.equal(pure.sanitizeAiText("   ", 100), "");
  assert.equal(pure.sanitizeAiText(null, 100), "");
  assert.equal(pure.sanitizeAiText(42, 100), "");
  const long = "汉".repeat(2000);
  assert.equal(Array.from(pure.sanitizeAiText(long, 1024)).length, 1024);
});

test("sanitizeCookieHeader: 过滤换行、限制 16 KiB", () => {
  assert.equal(pure.sanitizeCookieHeader("a=1; b=2"), "a=1; b=2");
  assert.equal(pure.sanitizeCookieHeader("a=1\r\nX-Injected: yes"), "");
  assert.equal(pure.sanitizeCookieHeader("   "), "");
  assert.equal(pure.sanitizeCookieHeader(null), "");
  const huge = Array.from({ length: 4000 }, (_, i) => "n" + i + "=" + "v".repeat(8)).join("; ");
  const header = pure.sanitizeCookieHeader(huge);
  assert.ok(new TextEncoder().encode(header).length <= 16 * 1024);
  assert.ok(header.startsWith("n0=v"));
});

test("resolveAiIncludeCredentials: 默认不发送，显式开关优先", () => {
  assert.equal(pure.resolveAiIncludeCredentials(undefined), false);
  assert.equal(pure.resolveAiIncludeCredentials(pure.normalizeSettings({})), false);
  assert.equal(pure.resolveAiIncludeCredentials({ aiIncludeCredentials: true }), true);
  assert.equal(pure.resolveAiIncludeCredentials({ aiIncludeCredentials: "true" }), true);
  // popup 复选框的显式状态优先于持久化设置
  assert.equal(pure.resolveAiIncludeCredentials({ aiIncludeCredentials: true }, false), false);
  assert.equal(pure.resolveAiIncludeCredentials({ aiIncludeCredentials: false }, true), true);
});

test("buildAiParseRequest: 字段清洗且仅在勾选凭证时携带 Cookie", () => {
  const base = {
    url: "https://example.com/page?x=1",
    title: "  Example\u0007 Page  ",
    sourcePage: "https://example.com/list",
    referer: "https://example.com/list",
    userAgent: "Mozilla/5.0 (test)",
    cookie: "sid=topsecret; theme=dark",
    launchGui: true,
  };
  assert.deepEqual(pure.buildAiParseRequest({ ...base, includeCredentials: false }), {
    type: "ai_parse",
    url: "https://example.com/page?x=1",
    title: "Example Page",
    source_page: "https://example.com/list",
    include_credentials: false,
    referer: "https://example.com/list",
    user_agent: "Mozilla/5.0 (test)",
    launch_gui: true,
  });
  const withCreds = pure.buildAiParseRequest({ ...base, includeCredentials: true });
  assert.equal(withCreds.include_credentials, true);
  assert.equal(withCreds.cookie, "sid=topsecret; theme=dark");
});

test("buildAiParseRequest: 拒绝非 http(s) 字段、空输入安全", () => {
  const message = pure.buildAiParseRequest({
    url: "file:///C:/secret.txt",
    sourcePage: "javascript:alert(1)",
    referer: "data:text/plain,x",
    includeCredentials: true,
    cookie: "a=1\r\nX-Injected: yes",
    title: "\u0000",
  });
  assert.deepEqual(message, { type: "ai_parse", url: "", include_credentials: true });
  assert.equal("cookie" in message, false);
  assert.deepEqual(pure.buildAiParseRequest(null), {
    type: "ai_parse",
    url: "",
    include_credentials: false,
  });
});

test("buildAiParseRequest: 页面快照仅在勾选凭证且 page_url 合法时输出", () => {
  const message = pure.buildAiParseRequest({
    url: "https://example.com/page?x=1",
    title: "Example Page",
    includeCredentials: true,
    pageUrl: "https://example.com/page?x=1",
    pageTitle: "  示例\u0007 页面 ",
    pageLinks: [
      "https://cdn.example.com/a.iso",
      "http://cdn.example.com/b.zip",
      "file:///C:/secret.txt",
      "javascript:alert(1)",
      "https://cdn.example.com/a.iso",
      "",
      42,
    ],
  });
  assert.equal(message.page_url, "https://example.com/page?x=1");
  assert.equal(message.page_title, "示例 页面");
  assert.deepEqual(message.page_links, [
    "https://cdn.example.com/a.iso",
    "http://cdn.example.com/b.zip",
  ]);
});

test("buildAiParseRequest: 未勾选凭证时不输出页面快照", () => {
  const message = pure.buildAiParseRequest({
    url: "https://example.com/page",
    includeCredentials: false,
    pageUrl: "https://example.com/page",
    pageTitle: "Example Page",
    pageLinks: ["https://cdn.example.com/a.iso"],
  });
  assert.equal("page_url" in message, false);
  assert.equal("page_title" in message, false);
  assert.equal("page_links" in message, false);
});

test("buildAiParseRequest: page_url 非法/超长时忽略整组快照", () => {
  const cases = [
    "file:///C:/secret.txt",
    "javascript:alert(1)",
    "data:text/plain,hi",
    "not a url",
    "",
    "https://example.com/" + "a".repeat(pure.MAX_PAGE_URL_BYTES + 1),
  ];
  for (const pageUrl of cases) {
    const message = pure.buildAiParseRequest({
      url: "https://example.com/page",
      includeCredentials: true,
      pageUrl,
      pageTitle: "Example Page",
      pageLinks: ["https://cdn.example.com/a.iso"],
    });
    assert.equal("page_url" in message, false, pageUrl.slice(0, 40));
    assert.equal("page_title" in message, false);
    assert.equal("page_links" in message, false);
  }
  // 有 page_url 但没有链接时输出空数组（页面快照存在，链接为空）。
  const noLinks = pure.buildAiParseRequest({
    url: "https://example.com/page",
    includeCredentials: true,
    pageUrl: "https://example.com/page",
  });
  assert.equal(noLinks.page_url, "https://example.com/page");
  assert.deepEqual(noLinks.page_links, []);
});

test("sanitizePageLinks: 去重保序、过滤非 http(s)、单条 ≤2048 字节、最多 300 条", () => {
  assert.deepEqual(
    pure.sanitizePageLinks([
      "https://a.example.com/1",
      " https://a.example.com/1 ",
      "https://a.example.com/2",
      "ftp://a.example.com/x",
      "data:text/plain,hi",
      "javascript:alert(1)",
      null,
      42,
      "/relative/path",
      "",
    ]),
    ["https://a.example.com/1", "https://a.example.com/2"]
  );
  assert.equal(pure.MAX_PAGE_LINKS, 300);
  const many = [];
  for (let index = 0; index < 400; index += 1) {
    many.push("https://cdn.example.com/file-" + index + ".bin");
  }
  assert.equal(pure.sanitizePageLinks(many).length, pure.MAX_PAGE_LINKS);

  const tooLong = "https://cdn.example.com/" + "a".repeat(pure.MAX_PAGE_LINK_BYTES);
  assert.equal(pure.utf8ByteLength(tooLong) > pure.MAX_PAGE_LINK_BYTES, true);
  assert.deepEqual(pure.sanitizePageLinks([tooLong, "https://ok.example.com/x"]), [
    "https://ok.example.com/x",
  ]);
  const justUnder = "https://cdn.example.com/" + "a".repeat(pure.MAX_PAGE_LINK_BYTES - 30);
  assert.ok(pure.utf8ByteLength(justUnder) <= pure.MAX_PAGE_LINK_BYTES);
  assert.deepEqual(pure.sanitizePageLinks([justUnder]), [justUnder]);
});

test("sanitizePageTitle / truncateUtf8Bytes: 清洗与字节级截断（不切多字节字符）", () => {
  assert.equal(pure.sanitizePageTitle("  a\u0000b  "), "a b");
  assert.equal(pure.sanitizePageTitle(null), "");
  const long = "汉".repeat(2000);
  const title = pure.sanitizePageTitle(long);
  assert.ok(pure.utf8ByteLength(title) <= pure.MAX_PAGE_TITLE_BYTES);
  assert.ok(Array.from(title).length <= pure.MAX_PAGE_TITLE_BYTES);
  assert.equal(pure.truncateUtf8Bytes("abc", 10), "abc");
  assert.equal(pure.truncateUtf8Bytes("汉字", 3), "汉");
  assert.equal(pure.truncateUtf8Bytes("汉字", 4), "汉");
});

test("buildAiParseRequest: 无 page 输入的输出与 v0.2 完全一致（向后兼容）", () => {
  const message = pure.buildAiParseRequest({
    url: "https://example.com/page",
    title: "Example",
    sourcePage: "https://example.com/",
    includeCredentials: true,
    cookie: "sid=secret",
    referer: "https://example.com/",
    userAgent: "UA",
    launchGui: true,
  });
  assert.deepEqual(message, {
    type: "ai_parse",
    url: "https://example.com/page",
    title: "Example",
    source_page: "https://example.com/",
    include_credentials: true,
    cookie: "sid=secret",
    referer: "https://example.com/",
    user_agent: "UA",
    launch_gui: true,
  });
});

test("formatAiParseResult: accepted / error / 未知响应", () => {
  assert.deepEqual(
    pure.formatAiParseResult({
      type: "ai_parse_accepted",
      id: 1,
      ok: true,
      request_id: 12,
      gui_running: true,
    }),
    {
      ok: true,
      requestId: 12,
      message: "已发送到 RDM（#12），解析结果在桌面端 AI 栏查看",
    }
  );
  assert.equal(
    pure.formatAiParseResult({ type: "ai_parse_accepted", ok: true }).message,
    "已发送到 RDM，解析结果在桌面端 AI 栏查看"
  );

  const unavailable = pure.formatAiParseResult({
    type: "error",
    code: "gui_unavailable",
    message: "ignored",
  });
  assert.equal(unavailable.ok, false);
  assert.equal(unavailable.code, "gui_unavailable");
  assert.match(unavailable.message, /RDM 未运行/);
  assert.match(unavailable.message, /不会排队/);

  const invalid = pure.formatAiParseResult({
    type: "error",
    code: "invalid_request",
    message: "bad url",
  });
  assert.match(invalid.message, /请求无效/);
  assert.match(invalid.message, /bad url/);

  const unknown = pure.formatAiParseResult(null);
  assert.equal(unknown.ok, false);
  assert.equal(unknown.code, "unexpected_response");
});

test("aiParseRecentEntry: 最近记录脱敏且状态正确", () => {
  const entry = pure.aiParseRecentEntry(
    { url: "https://example.com/page?token=secret", title: "Example Page" },
    { ok: true, message: "已发送到 RDM" }
  );
  assert.equal(entry.status, "ai");
  assert.equal(entry.url, "https://example.com/page");
  assert.equal(entry.fileName, "Example Page");
  assert.equal(entry.detail, "已发送到 RDM");

  const failed = pure.aiParseRecentEntry(
    { url: "https://example.com/page" },
    { ok: false, message: "AI 解析未发送：RDM 未运行" }
  );
  assert.equal(failed.status, "error");
  assert.equal(failed.detail, "AI 解析未发送：RDM 未运行");
});

test("AI 凭证设置：默认关闭且兼容旧数据", () => {
  assert.equal(pure.DEFAULT_SETTINGS.aiIncludeCredentials, false);
  assert.equal(pure.normalizeSettings(undefined).aiIncludeCredentials, false);
  assert.equal(pure.normalizeSettings({}).aiIncludeCredentials, false);
  assert.equal(pure.normalizeSettings({ aiIncludeCredentials: "true" }).aiIncludeCredentials, true);
});

// ---------------------------------------------------------------------------
// 下载进度（status）纯逻辑
// ---------------------------------------------------------------------------

test("normalizeDownloads: 丢弃坏条目、数值字段防御性回退", () => {
  assert.deepEqual(pure.normalizeDownloads(null), []);
  assert.deepEqual(pure.normalizeDownloads("nope"), []);
  assert.deepEqual(pure.normalizeDownloads([null, "nope", 42]), []);

  const list = pure.normalizeDownloads([
    { id: 42, state: "downloading" }, // id 不是字符串 → 丢弃
    { id: "", state: "downloading" }, // 空 id → 丢弃
    { id: "dl_bad", state: "exploded" }, // 状态非法 → 丢弃
    {
      id: " dl_2 ",
      fileName: "big.iso",
      state: "downloading",
      downloaded: 512,
      total: 1024,
      speedBps: 512.5,
      connections: 7.9,
    },
    {
      id: "dl_3",
      fileName: 42,
      state: "completed",
      downloaded: "10",
      total: null,
      speedBps: null,
      connections: "abc",
    },
  ]);
  assert.equal(list.length, 2);
  assert.deepEqual(list[0], {
    id: "dl_2",
    fileName: "big.iso",
    state: "downloading",
    downloaded: 512,
    total: 1024,
    speedBps: 512.5,
    connections: 7,
    activeConnections: 0,
    targetConnections: 0,
    peakConnections: 0,
  });
  assert.deepEqual(list[1], {
    id: "dl_3",
    fileName: "",
    state: "completed",
    downloaded: 10,
    total: null,
    speedBps: 0,
    connections: 0,
    activeConnections: 0,
    targetConnections: 0,
    peakConnections: 0,
  });
  // total <= 0 视为未知大小
  assert.equal(
    pure.normalizeDownload({ id: "x", state: "downloading", total: 0 }).total,
    null
  );
  assert.equal(pure.normalizeDownload({ id: "x", state: "downloading", total: -5 }).total, null);
  assert.equal(pure.normalizeDownload({ id: "x", state: "downloading" }).downloaded, 0);
});

test("matchDownload: 按 downloadId 匹配，旧记录/找不到返回 null", () => {
  const downloads = pure.normalizeDownloads([
    { id: "dl_1", state: "downloading" },
    { id: "dl_2", state: "completed" },
  ]);
  assert.equal(pure.matchDownload({ downloadId: "dl_1" }, downloads).id, "dl_1");
  assert.equal(pure.matchDownload({ downloadId: "dl_2" }, downloads).id, "dl_2");
  assert.equal(pure.matchDownload({ downloadId: "dl_9" }, downloads), null);
  // 旧记录没有 downloadId（或为空串）→ 静默不显示进度
  assert.equal(pure.matchDownload({ fileName: "old.iso" }, downloads), null);
  assert.equal(pure.matchDownload({ downloadId: "" }, downloads), null);
  assert.equal(pure.matchDownload({}, downloads), null);
  assert.equal(pure.matchDownload(null, downloads), null);
  assert.equal(pure.matchDownload({ downloadId: "dl_1" }, null), null);
});

test("progressPercent: 百分比、未知总量、完成态与坏数据", () => {
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 512, total: 1024 }), 50);
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 0, total: 1024 }), 0);
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 1, total: 3 }), 33);
  // 下载量超过总量时夹到 100
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 4096, total: 1024 }), 100);
  // 总大小未知 → 不确定态
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 512, total: null }), null);
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: 512, total: 0 }), null);
  assert.equal(pure.progressPercent({ state: "downloading", downloaded: "abc", total: 100 }), null);
  // 已完成固定 100%，即使 total 未知
  assert.equal(pure.progressPercent({ state: "completed", downloaded: 512, total: null }), 100);
  assert.equal(pure.progressPercent(null), null);
});

test("formatSpeed: 人类可读速度，0/未知返回空", () => {
  assert.equal(pure.formatSpeed(0), "");
  assert.equal(pure.formatSpeed(-100), "");
  assert.equal(pure.formatSpeed(NaN), "");
  assert.equal(pure.formatSpeed(null), "");
  assert.equal(pure.formatSpeed(512), "512 B/s");
  assert.equal(pure.formatSpeed(1536), "1.5 KiB/s");
  assert.equal(pure.formatSpeed(20 * 1024 * 1024), "20 MiB/s");
});

test("formatProgressText: 各 state 的展示文案", () => {
  const mib = 1024 * 1024;
  assert.equal(
    pure.formatProgressText({
      state: "downloading",
      downloaded: 50 * mib,
      total: 100 * mib,
      speedBps: 20 * mib,
    }),
    "50% · 50 MiB/100 MiB · 20 MiB/s"
  );
  // total 未知：只显示已下载量与速度（进度条走不确定态）
  assert.equal(
    pure.formatProgressText({
      state: "downloading",
      downloaded: 30 * mib,
      total: null,
      speedBps: 10 * mib,
    }),
    "已下载 30 MiB · 10 MiB/s"
  );
  assert.equal(
    pure.formatProgressText({ state: "downloading", downloaded: 30 * mib, total: null }),
    "已下载 30 MiB"
  );
  assert.equal(pure.formatProgressText({ state: "downloading" }), "下载中");

  assert.equal(
    pure.formatProgressText({
      state: "paused",
      downloaded: 50 * mib,
      total: 100 * mib,
      speedBps: 0,
    }),
    "已暂停 · 50% · 50 MiB/100 MiB"
  );
  assert.equal(
    pure.formatProgressText({ state: "paused", downloaded: 30 * mib, total: null }),
    "已暂停 · 已下载 30 MiB"
  );
  assert.equal(pure.formatProgressText({ state: "paused" }), "已暂停");

  assert.equal(pure.formatProgressText({ state: "completed", downloaded: 10, total: null }), "已完成 100%");
  assert.equal(pure.formatProgressText({ state: "failed", downloaded: 10, total: 100 }), "下载失败");
  assert.equal(pure.formatProgressText({ state: "cancelled", downloaded: 10, total: 100 }), "已取消");
  assert.equal(pure.formatProgressText({ state: "unknown" }), "");
  assert.equal(pure.formatProgressText(null), "");
  assert.equal(pure.formatProgressText("nope"), "");
});

// ---------------------------------------------------------------------------
// 「正在下载」区：线程三字段、行文案、active 筛选
// ---------------------------------------------------------------------------

test("normalizeDownload: 线程三字段防御性归一化（缺失/坏数据 → 0）", () => {
  const withThreads = pure.normalizeDownload({
    id: "dl_1",
    state: "downloading",
    connections: 16,
    activeConnections: 4.9,
    targetConnections: "8",
    peakConnections: -2,
  });
  assert.equal(withThreads.activeConnections, 4);
  assert.equal(withThreads.targetConnections, 8);
  assert.equal(withThreads.peakConnections, 0);

  // 旧宿主 / 旧 GUI：三字段缺失 → 0，不抛错
  const legacy = pure.normalizeDownload({ id: "dl_2", state: "paused" });
  assert.deepEqual(
    [legacy.activeConnections, legacy.targetConnections, legacy.peakConnections],
    [0, 0, 0]
  );
  // target=0 时线程文案回退到 connections（规划上限）
  assert.equal(pure.formatThreads(legacy), "0/0");
  assert.equal(
    pure.formatThreads(pure.normalizeDownload({ id: "dl_3", state: "downloading", connections: 8 })),
    "0/8"
  );
});

test("formatThreads: 活跃/目标，target=0 回退 connections，缺失字段兜底", () => {
  assert.equal(
    pure.formatThreads({ activeConnections: 4, targetConnections: 8, connections: 16 }),
    "4/8"
  );
  assert.equal(
    pure.formatThreads({ activeConnections: 0, targetConnections: 8, connections: 8 }),
    "0/8"
  );
  // target=0（未知动态目标）→ 用规划上限 connections
  assert.equal(
    pure.formatThreads({ activeConnections: 3, targetConnections: 0, connections: 12 }),
    "3/12"
  );
  // 全部缺失 → 0/0
  assert.equal(pure.formatThreads({}), "0/0");
  assert.equal(pure.formatThreads(null), "0/0");
  // paused 场景（宿主上报 active=0）
  assert.equal(
    pure.formatThreads({ state: "paused", activeConnections: 0, targetConnections: 8 }),
    "0/8"
  );
});

test("formatActiveText: downloading / total=null / paused / 非 active", () => {
  const mib = 1024 * 1024;
  assert.equal(
    pure.formatActiveText({
      state: "downloading",
      downloaded: 50 * mib,
      total: 100 * mib,
      speedBps: 20 * mib,
      connections: 8,
      activeConnections: 4,
      targetConnections: 8,
    }),
    "50% · 50 MiB / 100 MiB · 20 MiB/s · 线程 4/8"
  );
  // connections（规划上限）与动态目标不同且都 >0 → 追加「上限 N」
  assert.equal(
    pure.formatActiveText({
      state: "downloading",
      downloaded: 50 * mib,
      total: 100 * mib,
      speedBps: 20 * mib,
      connections: 16,
      activeConnections: 4,
      targetConnections: 8,
    }),
    "50% · 50 MiB / 100 MiB · 20 MiB/s · 线程 4/8 · 上限 16"
  );
  // total=null：不确定态，不显示百分比
  assert.equal(
    pure.formatActiveText({
      state: "downloading",
      downloaded: 30 * mib,
      total: null,
      speedBps: 10 * mib,
      connections: 8,
      activeConnections: 2,
      targetConnections: 8,
    }),
    "已下载 30 MiB · 10 MiB/s · 线程 2/8"
  );
  // paused：只显示暂停状态与线程
  assert.equal(
    pure.formatActiveText({
      state: "paused",
      downloaded: 50 * mib,
      total: 100 * mib,
      connections: 8,
      activeConnections: 0,
      targetConnections: 8,
    }),
    "已暂停 · 线程 0/8"
  );
  // 非 active 状态 / 空输入 → 无文案
  assert.equal(pure.formatActiveText({ state: "completed", downloaded: 1, total: 1 }), "");
  assert.equal(pure.formatActiveText({ state: "failed" }), "");
  assert.equal(pure.formatActiveText({ state: "cancelled" }), "");
  assert.equal(pure.formatActiveText(null), "");
  assert.equal(pure.formatActiveText("nope"), "");
});

test("pickActiveDownloads: 只保留 downloading/paused，最多 3 条并给出溢出数", () => {
  assert.equal(pure.MAX_ACTIVE_DOWNLOADS, 3);
  assert.deepEqual(pure.pickActiveDownloads(null), { visible: [], overflow: 0, total: 0 });
  assert.deepEqual(pure.pickActiveDownloads([null, {}, "x"]), {
    visible: [],
    overflow: 0,
    total: 0,
  });

  const downloads = pure.normalizeDownloads([
    { id: "dl_1", state: "downloading" },
    { id: "dl_2", state: "paused" },
    { id: "dl_3", state: "completed" },
    { id: "dl_4", state: "failed" },
    { id: "dl_5", state: "downloading" },
    { id: "dl_6", state: "cancelled" },
    { id: "dl_7", state: "paused" },
  ]);
  const picked = pure.pickActiveDownloads(downloads);
  assert.deepEqual(
    picked.visible.map((item) => item.id),
    ["dl_1", "dl_2", "dl_5"]
  );
  assert.equal(picked.total, 4);
  assert.equal(picked.overflow, 1);

  // 正好 3 条 → 无溢出
  const exact = pure.pickActiveDownloads(
    pure.normalizeDownloads([
      { id: "a", state: "downloading" },
      { id: "b", state: "paused" },
      { id: "c", state: "downloading" },
    ])
  );
  assert.equal(exact.visible.length, 3);
  assert.equal(exact.overflow, 0);
  assert.equal(exact.total, 3);
});
