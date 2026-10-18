/*!
 * RDM 下载接管 — 纯函数模块（无浏览器 API，可单元测试）。
 *
 * 该文件有两种加载方式：
 *   1. Firefox 后台事件页：manifest.json 的 background.scripts 先加载本文件，
 *      把 API 挂到 globalThis.RDMPure，再加载 background.js；
 *   2. Node 测试：`require("./lib/pure.js")` 直接拿到同一组函数，
 *      由 `node --test test/pure.test.js` 覆盖筛选/文件名/URL 等纯逻辑。
 *
 * 只放"输入 → 输出"的决策函数；所有 browser.* 调用都在 background.js / UI 里。
 */
(function (root) {
  "use strict";

  /** 扩展默认设置。与 options.html 表单字段一一对应。 */
  const DEFAULT_SETTINGS = Object.freeze({
    /** 总开关：关闭后完全不接管下载。 */
    enabled: true,
    /** 小于该大小的文件不接管（0 = 不限制）。 */
    minSizeBytes: 0,
    /** 非空时仅接管这些扩展名（小写、不带点）。 */
    allowExtensions: [],
    /** 永不接管的扩展名（优先级高于白名单）。 */
    blockExtensions: [],
    /** 默认连接数（1–32，宿主还会再夹一次）。 */
    connections: 8,
    /** 保存目录；空字符串表示使用 RDM 的默认目录。 */
    outputDir: "",
    /** 限速（KiB/s，0 = 不限速）。 */
    speedLimitKiBps: 0,
    /** GUI 未运行时是否让原生宿主自动启动它。 */
    autoStartGui: true,
    /** 是否把浏览器 Cookie 一并转交给 RDM（部分站点下载必需）。 */
    forwardCookies: true,
    /** 是否启用右键菜单「使用 RDM 下载」。 */
    contextMenu: true,
  });

  const MIN_CONNECTIONS = 1;
  const MAX_CONNECTIONS = 32;
  const MAX_LIST_ENTRIES = 64;
  const MAX_FILE_NAME_CHARS = 180;
  const MAX_OUTPUT_DIR_CHARS = 1024;
  const MAX_COOKIE_HEADER_BYTES = 16 * 1024;

  function clampInt(value, min, max, fallback) {
    const parsed = typeof value === "number" ? value : parseInt(value, 10);
    if (!Number.isFinite(parsed)) return fallback;
    return Math.min(max, Math.max(min, Math.trunc(parsed)));
  }

  function toBool(value, fallback) {
    if (typeof value === "boolean") return value;
    if (value === "true" || value === 1 || value === "1") return true;
    if (value === "false" || value === 0 || value === "0") return false;
    return fallback;
  }

  /** 去掉路径分隔符/非法字符，得到一个安全的单段文件名。 */
  function sanitizeFileName(raw) {
    if (typeof raw !== "string") return "";
    let name = raw.trim();
    if (!name) return "";
    // 同时处理 Windows 与 POSIX 分隔符；Windows 下载项 filename 是绝对路径。
    const parts = name.split(/[\\/]/).filter((part) => part.length > 0);
    name = parts.length > 0 ? parts[parts.length - 1] : name;
    let cleaned = "";
    for (const ch of name) {
      const code = ch.codePointAt(0);
      if (code < 0x20 || code === 0x7f) continue;
      if ('<>:"|?*'.includes(ch)) {
        cleaned += "_";
      } else {
        cleaned += ch;
      }
    }
    cleaned = cleaned.trim().replace(/[.\s]+$/u, "").trim();
    if (!cleaned || cleaned === "." || cleaned === "..") return "";
    return Array.from(cleaned).slice(0, MAX_FILE_NAME_CHARS).join("");
  }

  /** 小写扩展名（不含点）；没有扩展名返回 ""。 */
  function extensionOf(fileName) {
    if (typeof fileName !== "string") return "";
    const base = fileName.trim().split(/[\\/]/).pop() || "";
    const dot = base.lastIndexOf(".");
    if (dot <= 0 || dot === base.length - 1) return "";
    const ext = base.slice(dot + 1).toLowerCase();
    return /^[a-z0-9]{1,12}$/.test(ext) ? ext : "";
  }

  function isHttpUrl(url) {
    if (typeof url !== "string") return false;
    if (!/^https?:\/\//i.test(url.trim())) return false;
    try {
      const parsed = new URL(url.trim());
      return (parsed.protocol === "http:" || parsed.protocol === "https:") && !!parsed.host;
    } catch (_err) {
      return false;
    }
  }

  /** 从 URL 猜测文件名（URL 解码失败时退回原文）。 */
  function fileNameFromUrl(url) {
    if (typeof url !== "string") return "";
    try {
      const parsed = new URL(url.trim());
      const last = parsed.pathname.split("/").filter(Boolean).pop() || "";
      let decoded = last;
      try {
        decoded = decodeURIComponent(last);
      } catch (_err) {
        /* 保留原文 */
      }
      return sanitizeFileName(decoded);
    } catch (_err) {
      return "";
    }
  }

  /**
   * 解析 Content-Disposition 中的文件名。
   * 支持 RFC 5987 的 filename*=UTF-8''… 以及普通 filename="…"/filename=…。
   */
  function parseContentDispositionFilename(header) {
    if (typeof header !== "string" || !header) return "";
    let match = /filename\*\s*=\s*([^']*)'[^']*'([^;]+)/i.exec(header);
    if (match) {
      let value = match[2].trim().replace(/^"(.*)"$/s, "$1");
      try {
        value = decodeURIComponent(value);
      } catch (_err) {
        /* 保留原文 */
      }
      const cleaned = sanitizeFileName(value);
      if (cleaned) return cleaned;
    }
    match = /filename\s*=\s*(?:"([^"]*)"|([^;]+))/i.exec(header);
    if (match) {
      let value = (match[1] !== undefined ? match[1] : match[2] || "").trim();
      if (value) {
        try {
          value = decodeURIComponent(value);
        } catch (_err) {
          /* 保留原文 */
        }
        const cleaned = sanitizeFileName(value);
        if (cleaned) return cleaned;
      }
    }
    return "";
  }

  /** 规范化扩展名列表（去点、小写、去重、限长）。 */
  function normalizeExtensionList(value) {
    const input = Array.isArray(value)
      ? value
      : typeof value === "string"
        ? parseExtensionList(value)
        : [];
    const out = [];
    for (const entry of input) {
      if (typeof entry !== "string") continue;
      const ext = entry.trim().toLowerCase().replace(/^\.+/, "");
      if (!/^[a-z0-9]{1,12}$/.test(ext)) continue;
      if (!out.includes(ext)) out.push(ext);
      if (out.length >= MAX_LIST_ENTRIES) break;
    }
    return out;
  }

  /** 把用户输入（逗号/空格/换行/分号分隔）解析成扩展名列表。 */
  function parseExtensionList(text) {
    if (typeof text !== "string" || !text.trim()) return [];
    const seen = new Set();
    const out = [];
    for (const entry of text
      .split(/[\s,;，；、]+/)
      .map((value) => value.trim().toLowerCase().replace(/^\.+/, ""))
      .filter((value) => /^[a-z0-9]{1,12}$/.test(value))) {
      if (seen.has(entry)) continue;
      seen.add(entry);
      out.push(entry);
    }
    return out;
  }

  function formatExtensionList(list) {
    return Array.isArray(list) ? list.join(", ") : "";
  }

  /** 合并/校正来自 storage 或 options 页的设置，永远返回完整对象。 */
  function normalizeSettings(raw) {
    const input = raw && typeof raw === "object" ? raw : {};
    let outputDir = typeof input.outputDir === "string" ? input.outputDir.trim() : "";
    if (outputDir.length > MAX_OUTPUT_DIR_CHARS) outputDir = outputDir.slice(0, MAX_OUTPUT_DIR_CHARS);
    return {
      enabled: toBool(input.enabled, DEFAULT_SETTINGS.enabled),
      minSizeBytes: clampInt(input.minSizeBytes, 0, Number.MAX_SAFE_INTEGER, DEFAULT_SETTINGS.minSizeBytes),
      allowExtensions: normalizeExtensionList(input.allowExtensions),
      blockExtensions: normalizeExtensionList(input.blockExtensions),
      connections: clampInt(input.connections, MIN_CONNECTIONS, MAX_CONNECTIONS, DEFAULT_SETTINGS.connections),
      outputDir,
      speedLimitKiBps: clampInt(input.speedLimitKiBps, 0, 1024 * 1024, DEFAULT_SETTINGS.speedLimitKiBps),
      autoStartGui: toBool(input.autoStartGui, DEFAULT_SETTINGS.autoStartGui),
      forwardCookies: toBool(input.forwardCookies, DEFAULT_SETTINGS.forwardCookies),
      contextMenu: toBool(input.contextMenu, DEFAULT_SETTINGS.contextMenu),
    };
  }

  /**
   * 核心接管决策。
   *
   * @param {object} input
   * @param {string} input.url
   * @param {string} input.fileName 已解析/清理过的文件名（可能为空）
   * @param {number} input.sizeBytes 已知大小；-1 表示未知
   * @param {object} input.settings 已 normalizeSettings 的设置
   * @param {boolean} [input.fallbackPending] 是否是我们自己发起的回退下载
   * @returns {{intercept: boolean, reason: string}}
   */
  function shouldIntercept(input) {
    const settings = normalizeSettings(input.settings);
    const url = typeof input.url === "string" ? input.url : "";
    const fileName = sanitizeFileName(input.fileName || "");
    const sizeBytes = Number.isFinite(input.sizeBytes) ? input.sizeBytes : -1;

    if (!settings.enabled) return { intercept: false, reason: "disabled" };
    if (!isHttpUrl(url)) return { intercept: false, reason: "non_http" };
    if (input.fallbackPending) return { intercept: false, reason: "fallback" };

    const ext = extensionOf(fileName);
    if (settings.allowExtensions.length > 0) {
      if (!ext || !settings.allowExtensions.includes(ext)) {
        return { intercept: false, reason: "not_in_allow_list" };
      }
    }
    if (ext && settings.blockExtensions.includes(ext)) {
      return { intercept: false, reason: "blocked_extension" };
    }
    if (settings.minSizeBytes > 0 && sizeBytes >= 0 && sizeBytes < settings.minSizeBytes) {
      return { intercept: false, reason: "too_small" };
    }
    return { intercept: true, reason: "ok" };
  }

  /** 把 cookies API 结果拼成 Cookie 请求头（限制总长度）。 */
  function buildCookieHeader(cookies) {
    if (!Array.isArray(cookies) || cookies.length === 0) return "";
    const parts = [];
    let bytes = 0;
    for (const cookie of cookies) {
      if (!cookie || typeof cookie.name !== "string" || cookie.name === "") continue;
      const value = typeof cookie.value === "string" ? cookie.value : "";
      const pair = cookie.name + "=" + value;
      if (/[\r\n\u0000]/.test(pair)) continue;
      const pairBytes = new TextEncoder().encode(pair).length + 2;
      if (bytes + pairBytes > MAX_COOKIE_HEADER_BYTES) break;
      parts.push(pair);
      bytes += pairBytes;
    }
    return parts.join("; ");
  }

  /** 日志/最近记录用的脱敏 URL：去掉 query、fragment 与 userinfo。 */
  function redactUrl(url) {
    if (typeof url !== "string") return "";
    try {
      const parsed = new URL(url.trim());
      parsed.username = "";
      parsed.password = "";
      parsed.search = "";
      parsed.hash = "";
      return parsed.toString();
    } catch (_err) {
      return "";
    }
  }

  /** 人类可读的相对时间（中文）。 */
  function formatAge(timestamp, now) {
    const then = typeof timestamp === "number" ? timestamp : 0;
    const current = typeof now === "number" ? now : Date.now();
    const seconds = Math.max(0, Math.floor((current - then) / 1000));
    if (!then) return "";
    if (seconds < 5) return "刚刚";
    if (seconds < 60) return seconds + " 秒前";
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return minutes + " 分钟前";
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return hours + " 小时前";
    const days = Math.floor(hours / 24);
    return days + " 天前";
  }

  /** 字节数 → 人类可读大小（中文单位）。 */
  function formatBytes(bytes) {
    const value = typeof bytes === "number" ? bytes : Number(bytes);
    if (!Number.isFinite(value) || value < 0) return "未知";
    if (value < 1024) return value + " B";
    const units = ["KiB", "MiB", "GiB", "TiB"];
    let scaled = value;
    let unit = "B";
    for (const next of units) {
      if (scaled < 1024) break;
      scaled /= 1024;
      unit = next;
    }
    return (scaled >= 10 ? scaled.toFixed(0) : scaled.toFixed(1)) + " " + unit;
  }

  function parseMegabytesToBytes(text) {
    const value = parseFloat(String(text === undefined || text === null ? "" : text).trim());
    if (!Number.isFinite(value) || value <= 0) return 0;
    return Math.round(value * 1024 * 1024);
  }

  function formatMegabytesFromBytes(bytes) {
    const value = Number(bytes);
    if (!Number.isFinite(value) || value <= 0) return "";
    const mb = value / (1024 * 1024);
    return Number.isInteger(mb) ? String(mb) : mb.toFixed(1);
  }

  const api = {
    DEFAULT_SETTINGS,
    MAX_CONNECTIONS,
    MIN_CONNECTIONS,
    clampInt,
    sanitizeFileName,
    extensionOf,
    isHttpUrl,
    fileNameFromUrl,
    parseContentDispositionFilename,
    normalizeExtensionList,
    parseExtensionList,
    formatExtensionList,
    normalizeSettings,
    shouldIntercept,
    buildCookieHeader,
    redactUrl,
    formatAge,
    formatBytes,
    parseMegabytesToBytes,
    formatMegabytesFromBytes,
  };

  root.RDMPure = api;
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
})(typeof globalThis !== "undefined" ? globalThis : this);
