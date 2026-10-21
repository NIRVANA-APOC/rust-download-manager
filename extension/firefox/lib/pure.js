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
    /** 保存目录；空字符串表示使用 RDM 的默认目录。 */
    outputDir: "",
    /** 限速（KiB/s，0 = 不限速）。 */
    speedLimitKiBps: 0,
    /** GUI 未运行时是否让原生宿主自动启动它。 */
    autoStartGui: true,
    /** 是否把浏览器 Cookie 一并转交给 RDM（部分站点下载必需）。 */
    forwardCookies: true,
    /** AI 解析时是否附带 Cookie 凭证（默认关闭：只有用户显式勾选才发送）。 */
    aiIncludeCredentials: false,
    /** 是否启用右键菜单「使用 RDM 下载」。 */
    contextMenu: true,
  });

  const MAX_LIST_ENTRIES = 64;
  const MAX_FILE_NAME_CHARS = 180;
  const MAX_OUTPUT_DIR_CHARS = 1024;
  const MAX_COOKIE_HEADER_BYTES = 16 * 1024;
  const MAX_AI_TITLE_CHARS = 1024;
  const MAX_AI_USER_AGENT_CHARS = 1024;
  // 页面链接快照（v0.3，与 docs/extension-protocol.md 的 ai_parse 限制一致）。
  const MAX_PAGE_URL_BYTES = 4096;
  const MAX_PAGE_TITLE_BYTES = 1024;
  const MAX_PAGE_LINKS = 300;
  const MAX_PAGE_LINK_BYTES = 2048;

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
      outputDir,
      speedLimitKiBps: clampInt(input.speedLimitKiBps, 0, 1024 * 1024, DEFAULT_SETTINGS.speedLimitKiBps),
      autoStartGui: toBool(input.autoStartGui, DEFAULT_SETTINGS.autoStartGui),
      forwardCookies: toBool(input.forwardCookies, DEFAULT_SETTINGS.forwardCookies),
      aiIncludeCredentials: toBool(
        input.aiIncludeCredentials,
        DEFAULT_SETTINGS.aiIncludeCredentials
      ),
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

  // -------------------------------------------------------------------------
  // AI 解析（ai_parse）：请求构造、凭证开关、结果文案
  // -------------------------------------------------------------------------

  /** 清理 AI 解析用的自由文本（标题 / UA）：去控制字符、折叠空白、限长。 */
  function sanitizeAiText(raw, maxChars) {
    if (typeof raw !== "string") return "";
    let text = "";
    for (const ch of raw) {
      const code = ch.codePointAt(0);
      if (code < 0x20 || code === 0x7f) {
        if (text && !text.endsWith(" ")) text += " ";
        continue;
      }
      text += ch;
    }
    text = text.replace(/\s+/gu, " ").trim();
    if (!text) return "";
    return Array.from(text).slice(0, maxChars).join("");
  }

  /** 清理一段原始 Cookie 头：过滤 CR/LF、限制字节数（超出时按条裁剪）。 */
  function sanitizeCookieHeader(raw) {
    if (typeof raw !== "string") return "";
    const text = raw.trim();
    if (!text || /[\r\n\u0000]/.test(text)) return "";
    const encoder = new TextEncoder();
    if (encoder.encode(text).length <= MAX_COOKIE_HEADER_BYTES) return text;
    const parts = [];
    let used = 0;
    for (const piece of text.split(";")) {
      const pair = piece.trim();
      if (!pair) continue;
      const bytes = encoder.encode(pair).length + 2;
      if (used + bytes > MAX_COOKIE_HEADER_BYTES) break;
      parts.push(pair);
      used += bytes;
    }
    return parts.join("; ");
  }

  /** UTF-8 字节数（协议里的长度限制都是字节数）。 */
  function utf8ByteLength(text) {
    return new TextEncoder().encode(String(text)).length;
  }

  /** 按 UTF-8 字节数截断（不会切断多字节字符）。 */
  function truncateUtf8Bytes(text, maxBytes) {
    const value = String(text);
    if (utf8ByteLength(value) <= maxBytes) return value;
    const encoder = new TextEncoder();
    let out = "";
    let used = 0;
    for (const ch of value) {
      const bytes = encoder.encode(ch).length;
      if (used + bytes > maxBytes) break;
      out += ch;
      used += bytes;
    }
    return out;
  }

  /**
   * 页面链接快照（v0.3）：只有 `include_credentials === true` 时才离开浏览器，
   * 且仅保留 http(s) 绝对页面 URL。
   */
  function sanitizePageUrl(raw) {
    if (typeof raw !== "string") return "";
    const url = raw.trim();
    if (!isHttpUrl(url)) return "";
    if (utf8ByteLength(url) > MAX_PAGE_URL_BYTES) return "";
    return url;
  }

  /** 页面标题：清洗控制字符/空白，按字符与 UTF-8 字节双重截断。 */
  function sanitizePageTitle(raw) {
    const text = sanitizeAiText(raw, MAX_PAGE_TITLE_BYTES);
    if (!text) return "";
    return truncateUtf8Bytes(text, MAX_PAGE_TITLE_BYTES);
  }

  /**
   * 页面链接：只保留 http(s) 绝对 URL，去重保序，最多 MAX_PAGE_LINKS 条，
   * 单条 ≤ MAX_PAGE_LINK_BYTES 字节；非法/超限项直接丢弃。
   */
  function sanitizePageLinks(raw) {
    if (!Array.isArray(raw)) return [];
    const links = [];
    const seen = new Set();
    for (const item of raw) {
      if (typeof item !== "string") continue;
      const link = item.trim();
      if (!isHttpUrl(link)) continue;
      if (utf8ByteLength(link) > MAX_PAGE_LINK_BYTES) continue;
      if (seen.has(link)) continue;
      seen.add(link);
      links.push(link);
      if (links.length >= MAX_PAGE_LINKS) break;
    }
    return links;
  }

  /**
   * 组装可选页面快照字段（snake_case，供 ai_parse 消息使用）。
   *
   * 返回 `null` 表示没有有效 `page_url`（整组快照丢弃，links/title 一并忽略）。
   * 输入使用 camelCase（与 background 的 executeScript 结果映射一致）。
   */
  function sanitizePageSnapshot(input) {
    const source = input && typeof input === "object" ? input : {};
    const pageUrl = sanitizePageUrl(source.pageUrl);
    if (!pageUrl) return null;
    const snapshot = {
      page_url: pageUrl,
      page_links: sanitizePageLinks(source.pageLinks),
    };
    const title = sanitizePageTitle(source.pageTitle);
    if (title) snapshot.page_title = title;
    return snapshot;
  }

  /**
   * AI 解析是否附带凭证。
   *
   * `requested` 是 popup 复选框的显式状态（优先）；缺省时读取持久化设置。
   * 默认 false：只有用户显式勾选，Cookie 才会离开浏览器。
   */
  function resolveAiIncludeCredentials(settings, requested) {
    if (typeof requested === "boolean") return requested;
    return normalizeSettings(settings).aiIncludeCredentials === true;
  }

  /**
   * 构造发给原生宿主的 `ai_parse` 消息（消息 id 由 background 补）。
   *
   * 只有 `includeCredentials === true` 时才携带 cookie；所有字段都在这里
   * 清洗（URL 必须是 http(s)、标题/UA 去掉控制字符、Cookie 限 16 KiB）。
   */
  function buildAiParseRequest(input) {
    const source = input && typeof input === "object" ? input : {};
    const url = isHttpUrl(source.url) ? String(source.url).trim() : "";
    const message = { type: "ai_parse", url };

    const title = sanitizeAiText(source.title, MAX_AI_TITLE_CHARS);
    if (title) message.title = title;

    const sourcePage = isHttpUrl(source.sourcePage) ? String(source.sourcePage).trim() : "";
    if (sourcePage) message.source_page = sourcePage;

    const includeCredentials = source.includeCredentials === true;
    message.include_credentials = includeCredentials;
    if (includeCredentials) {
      const cookie = sanitizeCookieHeader(source.cookie);
      if (cookie) message.cookie = cookie;
      // v0.3 页面链接快照：与凭证同意捆绑，且必须有合法 page_url 才输出。
      const snapshot = sanitizePageSnapshot({
        pageUrl: source.pageUrl,
        pageTitle: source.pageTitle,
        pageLinks: source.pageLinks,
      });
      if (snapshot) Object.assign(message, snapshot);
    }

    const referer = isHttpUrl(source.referer) ? String(source.referer).trim() : "";
    if (referer) message.referer = referer;

    const userAgent = sanitizeAiText(source.userAgent, MAX_AI_USER_AGENT_CHARS);
    if (userAgent) message.user_agent = userAgent;

    if (typeof source.launchGui === "boolean") message.launch_gui = source.launchGui;
    return message;
  }

  /** ai_parse 错误码 → 面向用户的简短中文说明。 */
  function aiParseErrorText(code, detail) {
    switch (String(code || "")) {
      case "gui_unavailable":
        return "RDM 未运行（AI 解析不会排队，请先启动 RDM）";
      case "invalid_request":
        return "请求无效" + (detail ? "：" + String(detail) : "");
      case "unauthorized":
        return "IPC 鉴权失败，请重启 RDM";
      case "internal":
        return "RDM 内部错误" + (detail ? "：" + String(detail) : "");
      default:
        return detail ? String(detail) : "未知错误";
    }
  }

  /**
   * ai_parse 响应 → popup 展示文案。
   * @returns {{ok: boolean, message: string, requestId?: number, code?: string}}
   */
  function formatAiParseResult(response) {
    const reply = response && typeof response === "object" ? response : {};
    if (reply.type === "ai_parse_accepted") {
      const requestId = Number.isFinite(reply.request_id) ? reply.request_id : 0;
      const suffix = requestId > 0 ? "（#" + requestId + "）" : "";
      return {
        ok: true,
        requestId,
        message: "已发送到 RDM" + suffix + "，解析结果在桌面端 AI 栏查看",
      };
    }
    if (reply.type === "error") {
      return {
        ok: false,
        code: typeof reply.code === "string" ? reply.code : "unknown",
        message: "AI 解析未发送：" + aiParseErrorText(reply.code, reply.message),
      };
    }
    return {
      ok: false,
      code: "unexpected_response",
      message: "AI 解析未发送：宿主响应异常",
    };
  }

  /** 右键 AI 解析写入「最近」列表的条目（不含 Cookie；标题限 80 字符）。 */
  function aiParseRecentEntry(input, result) {
    const source = input && typeof input === "object" ? input : {};
    const reply = result && typeof result === "object" ? result : {};
    const title = sanitizeAiText(source.title, 80);
    return {
      fileName: title || redactUrl(source.url) || "AI 解析",
      url: redactUrl(source.url),
      status: reply.ok === true ? "ai" : "error",
      detail:
        typeof reply.message === "string" && reply.message
          ? reply.message
          : reply.ok === true
            ? "已发送到 RDM"
            : "AI 解析失败",
    };
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

  // -------------------------------------------------------------------------
  // 下载进度（status）：归一化、匹配与进度文案
  // -------------------------------------------------------------------------

  /** 宿主 status 消息中允许的下载状态（语义见 docs/extension-protocol.md §1.3）。 */
  const DOWNLOAD_STATES = Object.freeze([
    "downloading",
    "paused",
    "completed",
    "failed",
    "cancelled",
  ]);

  /** 单次 status 快照最多保留的下载条数（防止异常数据拖慢 popup）。 */
  const MAX_DOWNLOADS = 200;

  /** 「正在下载」区最多展示的条目数（超出在 popup 里显示「还有 N 个…」）。 */
  const MAX_ACTIVE_DOWNLOADS = 3;

  /** 宽松数值转换：只接受 number 或数字字符串，其余返回 fallback（不抛错）。 */
  function toFiniteNumber(value, fallback) {
    if (typeof value === "number") return Number.isFinite(value) ? value : fallback;
    if (typeof value === "string" && value.trim()) {
      const parsed = Number(value);
      return Number.isFinite(parsed) ? parsed : fallback;
    }
    return fallback;
  }

  /**
   * 归一化单条下载（入参是 background 已转成 camelCase 的原始对象）。
   *
   * - id / state 非法 → 返回 null（整条丢弃，绝不抛错）；
   * - downloaded / total / speedBps / connections 以及线程三字段
   *   （activeConnections / targetConnections / peakConnections）做类型防御：
   *   非数字按 0 处理，total <= 0 视为「未知大小」存 null。
   */
  function normalizeDownload(raw) {
    if (!raw || typeof raw !== "object") return null;
    const id = typeof raw.id === "string" ? raw.id.trim() : "";
    if (!id) return null;
    const state = typeof raw.state === "string" ? raw.state : "";
    if (!DOWNLOAD_STATES.includes(state)) return null;
    const totalValue = toFiniteNumber(raw.total, 0);
    return {
      id,
      fileName: typeof raw.fileName === "string" ? raw.fileName : "",
      state,
      downloaded: Math.max(0, toFiniteNumber(raw.downloaded, 0)),
      total: totalValue > 0 ? totalValue : null,
      speedBps: Math.max(0, toFiniteNumber(raw.speedBps, 0)),
      connections: Math.max(0, Math.trunc(toFiniteNumber(raw.connections, 0))),
      // 旧宿主不返回这三个字段 → 0 兜底（popup 据此降级显示）。
      activeConnections: Math.max(0, Math.trunc(toFiniteNumber(raw.activeConnections, 0))),
      targetConnections: Math.max(0, Math.trunc(toFiniteNumber(raw.targetConnections, 0))),
      peakConnections: Math.max(0, Math.trunc(toFiniteNumber(raw.peakConnections, 0))),
    };
  }

  /** 归一化 status.downloads 列表：丢弃坏条目、限制条数。 */
  function normalizeDownloads(list) {
    if (!Array.isArray(list)) return [];
    const out = [];
    for (const raw of list) {
      const item = normalizeDownload(raw);
      if (item) out.push(item);
      if (out.length >= MAX_DOWNLOADS) break;
    }
    return out;
  }

  /**
   * 用最近记录的 downloadId 在 status.downloads 里找对应下载。
   * 旧记录（downloadId 缺失/空串）或找不到时返回 null —— 静默不显示进度。
   */
  function matchDownload(entry, downloads) {
    if (!entry || typeof entry !== "object") return null;
    const id = typeof entry.downloadId === "string" ? entry.downloadId : "";
    if (!id || !Array.isArray(downloads)) return null;
    for (const item of downloads) {
      if (item && typeof item === "object" && item.id === id) return item;
    }
    return null;
  }

  /** 进度百分比（0–100 的整数）；total 未知/非法时返回 null（不确定态）。 */
  function progressPercent(download) {
    if (!download || typeof download !== "object") return null;
    if (download.state === "completed") return 100;
    const downloaded = toFiniteNumber(download.downloaded, NaN);
    const total = toFiniteNumber(download.total, NaN);
    if (!Number.isFinite(downloaded) || !Number.isFinite(total) || total <= 0) return null;
    const percent = Math.floor((Math.max(0, downloaded) / total) * 100);
    return Math.max(0, Math.min(100, percent));
  }

  /** 速度 → 「1.5 MiB/s」；未知/0/负数返回 ""。 */
  function formatSpeed(bytesPerSecond) {
    const speed = toFiniteNumber(bytesPerSecond, 0);
    if (!Number.isFinite(speed) || speed <= 0) return "";
    return formatBytes(speed) + "/s";
  }

  /**
   * 下载状态 → popup 进度文案（各 state 的显示规则见 popup.js）：
   *
   * - downloading：「45% · 4.5 MiB/10 MiB · 1.5 MiB/s」；
   *   total 未知时：「已下载 4.5 MiB · 1.5 MiB/s」（进度条走不确定态）。
   * - paused：「已暂停 · 45% · 4.5 MiB/10 MiB」。
   * - completed：「已完成 100%」。
   * - failed：「下载失败」；cancelled：「已取消」。
   * - 其他/空输入：""。
   */
  function formatProgressText(download) {
    if (!download || typeof download !== "object") return "";
    const state = typeof download.state === "string" ? download.state : "";
    const percent = progressPercent(download);
    const speed = formatSpeed(download.speedBps);
    const downloaded = toFiniteNumber(download.downloaded, NaN);
    const total = toFiniteNumber(download.total, NaN);
    const hasDownloaded = Number.isFinite(downloaded) && downloaded >= 0;
    const hasTotal = Number.isFinite(total) && total > 0;

    if (state === "completed") return "已完成 100%";
    if (state === "failed") return "下载失败";
    if (state === "cancelled") return "已取消";

    if (state === "paused") {
      const parts = ["已暂停"];
      if (percent !== null && hasTotal) {
        parts.push(percent + "%");
        parts.push(formatBytes(downloaded) + "/" + formatBytes(total));
      } else if (hasDownloaded && downloaded > 0) {
        parts.push("已下载 " + formatBytes(downloaded));
      }
      return parts.join(" · ");
    }

    if (state === "downloading") {
      if (percent !== null && hasTotal) {
        const parts = [percent + "%", formatBytes(downloaded) + "/" + formatBytes(total)];
        if (speed) parts.push(speed);
        return parts.join(" · ");
      }
      const parts = [];
      if (hasDownloaded) parts.push("已下载 " + formatBytes(downloaded));
      if (speed) parts.push(speed);
      return parts.length > 0 ? parts.join(" · ") : "下载中";
    }

    return "";
  }

  // -------------------------------------------------------------------------
  // 「正在下载」区：线程数、行文案与筛选
  // -------------------------------------------------------------------------

  /** 线程目标值：targetConnections 优先；为 0（未知）时退回规划上限 connections。 */
  function threadTarget(download) {
    if (!download || typeof download !== "object") return 0;
    const target = Math.max(0, Math.trunc(toFiniteNumber(download.targetConnections, 0)));
    if (target > 0) return target;
    return Math.max(0, Math.trunc(toFiniteNumber(download.connections, 0)));
  }

  /**
   * 线程数文案「当前活跃 / 目标」。
   * 例："4/8"（动态规划目标 8、当前 4 个 worker）、"0/8"（暂停/无活跃 worker）。
   * 缺失字段按 0 兜底（旧宿主不返回 → "0/规划上限" 或 "0/0"）。
   */
  function formatThreads(download) {
    if (!download || typeof download !== "object") return "0/0";
    const active = Math.max(0, Math.trunc(toFiniteNumber(download.activeConnections, 0)));
    return active + "/" + threadTarget(download);
  }

  /**
   * 「正在下载」区一行的进度文案：
   *
   * - downloading：「42% · 12.0 MiB / 24.0 MiB · 5.0 MiB/s · 线程 4/8」；
   *   total 未知时：「已下载 12.0 MiB · 5.0 MiB/s · 线程 4/8」（进度条走不确定态）。
   * - paused：「已暂停 · 线程 0/8」。
   * - 其他状态 / 空输入：""。
   *
   * 若规划上限 connections 与目标 targetConnections 都 >0 且不相等，末尾追加
   * 「· 上限 16」，提示用户实际规划上限与动态目标不同。
   */
  function formatActiveText(download) {
    if (!download || typeof download !== "object") return "";
    const state = typeof download.state === "string" ? download.state : "";
    const threads = "线程 " + formatThreads(download);
    if (state === "paused") return "已暂停 · " + threads;
    if (state !== "downloading") return "";

    const percent = progressPercent(download);
    const speed = formatSpeed(download.speedBps);
    const downloaded = toFiniteNumber(download.downloaded, NaN);
    const total = toFiniteNumber(download.total, NaN);
    const hasDownloaded = Number.isFinite(downloaded) && downloaded >= 0;
    const hasTotal = Number.isFinite(total) && total > 0;
    const connections = Math.max(0, Math.trunc(toFiniteNumber(download.connections, 0)));
    const target = threadTarget(download);

    const parts = [];
    if (percent !== null && hasTotal) {
      parts.push(percent + "%");
      parts.push(formatBytes(downloaded) + " / " + formatBytes(total));
    } else if (hasDownloaded) {
      parts.push("已下载 " + formatBytes(downloaded));
    }
    if (speed) parts.push(speed);
    parts.push(threads);
    if (connections > 0 && target > 0 && connections !== target) {
      parts.push("上限 " + connections);
    }
    return parts.join(" · ");
  }

  /**
   * 从 status.downloads 里挑出「正在下载」条目（downloading / paused）。
   * 最多返回 MAX_ACTIVE_DOWNLOADS 条，其余计入 overflow；total 是过滤后的总数。
   *
   * @returns {{visible: object[], overflow: number, total: number}}
   */
  function pickActiveDownloads(downloads) {
    const list = Array.isArray(downloads) ? downloads : [];
    const active = [];
    for (const item of list) {
      if (!item || typeof item !== "object") continue;
      if (item.state !== "downloading" && item.state !== "paused") continue;
      active.push(item);
    }
    return {
      visible: active.slice(0, MAX_ACTIVE_DOWNLOADS),
      overflow: Math.max(0, active.length - MAX_ACTIVE_DOWNLOADS),
      total: active.length,
    };
  }

  const api = {
    DEFAULT_SETTINGS,
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
    sanitizeAiText,
    sanitizeCookieHeader,
    utf8ByteLength,
    truncateUtf8Bytes,
    sanitizePageUrl,
    sanitizePageTitle,
    sanitizePageLinks,
    sanitizePageSnapshot,
    resolveAiIncludeCredentials,
    buildAiParseRequest,
    aiParseErrorText,
    formatAiParseResult,
    aiParseRecentEntry,
    redactUrl,
    formatAge,
    formatBytes,
    parseMegabytesToBytes,
    formatMegabytesFromBytes,
    DOWNLOAD_STATES,
    MAX_DOWNLOADS,
    MAX_ACTIVE_DOWNLOADS,
    MAX_PAGE_URL_BYTES,
    MAX_PAGE_TITLE_BYTES,
    MAX_PAGE_LINKS,
    MAX_PAGE_LINK_BYTES,
    normalizeDownload,
    normalizeDownloads,
    matchDownload,
    progressPercent,
    formatSpeed,
    formatProgressText,
    threadTarget,
    formatThreads,
    formatActiveText,
    pickActiveDownloads,
  };

  root.RDMPure = api;
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
})(typeof globalThis !== "undefined" ? globalThis : this);
