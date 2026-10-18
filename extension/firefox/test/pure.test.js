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

test("normalizeSettings: 永远返回完整且被夹紧的设置", () => {
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
  assert.equal(settings.connections, 32);
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
    "connections",
    "outputDir",
    "speedLimitKiBps",
    "autoStartGui",
    "forwardCookies",
    "contextMenu",
  ]) {
    assert.ok(key in pure.DEFAULT_SETTINGS, "missing default setting: " + key);
  }
});
