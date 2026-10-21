/*!
 * RDM 下载接管 — Firefox 后台事件页（Manifest V3）
 *
 * 职责：
 *   1. 用 webRequest 观察 http(s) 请求/响应头（MV3 下只能观察，不能 block）；
 *   2. 监听 downloads.onCreated，按设置决定是否接管：
 *        原生宿主可用 → 取消并抹掉浏览器下载 → native messaging 转交 RDM；
 *        宿主不可用/转交失败 → 重新发起浏览器下载（并防止再次拦截形成死循环）；
 *   3. 维护原生宿主 / GUI 在线状态与工具栏徽章；
 *   4. 右键菜单「使用 RDM 下载」/「用 RDM AI 解析此页」；
 *   5. 为 popup / options 提供消息接口（含 AI 解析 ai_parse）。
 *
 * 设计约束（Firefox MV3）：
 *   - background 使用 background.scripts（Firefox 不支持 service worker）；
 *   - webRequest 没有 blocking 能力，所以接管只能"先发生再取消"；
 *   - 监听器必须在顶层同步注册，否则事件页被唤醒后可能错过事件。
 *
 * 隐私：不记录 Cookie / URL query / 请求头内容。日志与"最近转交"只保存
 * 脱敏 URL、文件名、结果状态。
 */
"use strict";

/* global RDMPure */
const {
  normalizeSettings,
  shouldIntercept,
  sanitizeFileName,
  isHttpUrl,
  fileNameFromUrl,
  parseContentDispositionFilename,
  buildCookieHeader,
  redactUrl,
  formatBytes,
  resolveAiIncludeCredentials,
  buildAiParseRequest,
  formatAiParseResult,
  aiParseRecentEntry,
  normalizeDownloads,
} = RDMPure;

const HOST_NAME = "com.rdm.download";
const SETTINGS_KEY = "rdm.settings";
const RECENT_KEY = "rdm.recent";
const STATUS_KEY = "rdm.status";
const PING_ALARM = "rdm.status.ping";
const MENU_ID = "rdm-download-with";
const AI_MENU_ID = "rdm-ai-parse-page";
const MAX_RECENT = 20;
const PING_TIMEOUT_MS = 3000;
/** popup 每秒轮询一次下载进度；宿主/GUI 正常时应远快于该预算。 */
const STATUS_TIMEOUT_MS = 3000;
/** GUI 未被启动时，宿主最多轮询 8 秒，这里留足余量。 */
const ENQUEUE_TIMEOUT_MS = 16000;
/** AI 解析同样可能触发 GUI 启动，给与 enqueue 相同的预算。 */
const AI_PARSE_TIMEOUT_MS = 16000;
const CACHE_TTL_MS = 90 * 1000;
const MAX_CACHE_ENTRIES = 80;
const FALLBACK_GUARD_TTL_MS = 30 * 1000;
const FALLBACK_GUARD_MAX = 20;
const MAX_DEBUG_LINES = 40;

const HOST_STATES = ["unknown", "ok", "missing", "error"];

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function truncate(text, maxLength) {
  const value = String(text === undefined || text === null ? "" : text);
  return value.length <= maxLength ? value : value.slice(0, maxLength - 1) + "…";
}

const debugLines = [];

function debug(message) {
  const line = "[RDM " + new Date().toISOString() + "] " + message;
  debugLines.push(line);
  if (debugLines.length > MAX_DEBUG_LINES) debugLines.shift();
  // 扩展自己的控制台：about:debugging → 检查 → 控制台
  console.log(line);
}

async function storageGet(key) {
  try {
    return (await browser.storage.local.get(key)) || {};
  } catch (err) {
    debug("读取 storage 失败: " + err);
    return {};
  }
}

async function storageSet(values) {
  try {
    await browser.storage.local.set(values);
  } catch (err) {
    debug("写入 storage 失败: " + err);
  }
}

// ---------------------------------------------------------------------------
// 设置 / 状态 / 最近转交
// ---------------------------------------------------------------------------

let cachedSettings = null;
let settingsLoadPromise = null;
let status = { host: "unknown", gui: false, detail: "", checkedAt: 0, hostVersion: "" };
let recent = null;

async function loadSettings() {
  if (cachedSettings) return cachedSettings;
  if (!settingsLoadPromise) {
    settingsLoadPromise = (async () => {
      const stored = await storageGet(SETTINGS_KEY);
      cachedSettings = normalizeSettings(stored[SETTINGS_KEY]);
      return cachedSettings;
    })().finally(() => {
      settingsLoadPromise = null;
    });
  }
  return settingsLoadPromise;
}

async function saveSettings(partial) {
  const current = await loadSettings();
  const merged = normalizeSettings(Object.assign({}, current, partial || {}));
  cachedSettings = merged;
  await storageSet({ [SETTINGS_KEY]: merged });
  await ensureContextMenu(merged);
  updateBadge();
  return merged;
}

function normalizeStoredStatus(raw) {
  const input = raw && typeof raw === "object" ? raw : {};
  const host = HOST_STATES.includes(input.host) ? input.host : "unknown";
  return {
    host,
    gui: input.gui === true,
    detail: typeof input.detail === "string" ? input.detail : "",
    checkedAt: typeof input.checkedAt === "number" ? input.checkedAt : 0,
    hostVersion: typeof input.hostVersion === "string" ? input.hostVersion : "",
  };
}

async function loadStoredStatus() {
  const stored = await storageGet(STATUS_KEY);
  return normalizeStoredStatus(stored[STATUS_KEY]);
}

let statusPersistTimer = null;

function setStatus(host, gui, detail, hostVersion) {
  status = {
    host,
    gui: gui === true,
    detail: detail || "",
    checkedAt: Date.now(),
    hostVersion: hostVersion || status.hostVersion || "",
  };
  updateBadge();
  if (statusPersistTimer) clearTimeout(statusPersistTimer);
  statusPersistTimer = setTimeout(() => {
    statusPersistTimer = null;
    storageSet({ [STATUS_KEY]: status });
  }, 250);
}

/** 「最近转交」允许的状态；未知值按 error 处理（保持旧行为）。 */
const RECENT_STATES = ["handed", "queued", "fallback", "ai", "error"];

/**
 * 归一化一条「最近转交」记录。
 *
 * 旧版本 storage 里的记录没有 `downloadId` 字段，这里统一补 ""，popup 据此
 * 静默跳过进度显示（不报错、不显示错误进度条）。
 */
function normalizeRecentEntry(raw) {
  const entry = raw && typeof raw === "object" ? raw : {};
  return {
    at: typeof entry.at === "number" && Number.isFinite(entry.at) ? entry.at : 0,
    fileName: typeof entry.fileName === "string" ? entry.fileName : "",
    url: typeof entry.url === "string" ? entry.url : "",
    status: RECENT_STATES.includes(entry.status) ? entry.status : "error",
    detail: typeof entry.detail === "string" ? entry.detail : "",
    downloadId: typeof entry.downloadId === "string" ? entry.downloadId : "",
  };
}

async function loadRecent() {
  if (recent) return recent;
  const stored = await storageGet(RECENT_KEY);
  const list = Array.isArray(stored[RECENT_KEY]) ? stored[RECENT_KEY] : [];
  recent = list.map(normalizeRecentEntry);
  return recent;
}

async function pushRecent(entry) {
  const list = (await loadRecent()).slice();
  list.unshift(normalizeRecentEntry(Object.assign({ at: Date.now() }, entry)));
  recent = list.slice(0, MAX_RECENT);
  await storageSet({ [RECENT_KEY]: recent });
}

async function clearRecent() {
  recent = [];
  await storageSet({ [RECENT_KEY]: [] });
}

// ---------------------------------------------------------------------------
// 原生消息客户端（持久 Port；断开后下次请求自动重连）
// ---------------------------------------------------------------------------

const nativeClient = {
  port: null,
  nextId: 1,
  pending: new Map(),

  ensurePort() {
    if (this.port) return this.port;
    const port = browser.runtime.connectNative(HOST_NAME);
    port.onMessage.addListener((message) => this.handleMessage(message));
    port.onDisconnect.addListener(() => this.handleDisconnect(port));
    this.port = port;
    return port;
  },

  handleMessage(message) {
    if (!message || typeof message !== "object") return;
    const id = typeof message.id === "number" ? message.id : 0;
    const waiter = this.pending.get(id);
    if (!waiter) {
      debug("收到没有等待者的原生消息: id=" + id);
      return;
    }
    this.pending.delete(id);
    clearTimeout(waiter.timer);
    waiter.resolve(message);
  },

  handleDisconnect(port) {
    if (this.port === port) this.port = null;
    let message = "原生宿主已断开";
    if (port && port.error && port.error.message) {
      message = port.error.message;
    } else {
      const lastError = browser.runtime.lastError;
      if (lastError && lastError.message) message = lastError.message;
    }
    for (const [, waiter] of this.pending) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error(message));
    }
    this.pending.clear();
    const classified = classifyHostError(message);
    setStatus(classified.host, false, classified.detail);
    debug("原生宿主连接断开: " + message);
  },

  request(message, timeoutMs) {
    let port;
    try {
      port = this.ensurePort();
    } catch (err) {
      return Promise.reject(err instanceof Error ? err : new Error(String(err)));
    }
    const id = this.nextId++;
    const payload = Object.assign({}, message, { id });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error("原生宿主响应超时"));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        port.postMessage(payload);
      } catch (err) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(err);
      }
    });
  },

  disconnect() {
    if (!this.port) return;
    const port = this.port;
    this.port = null;
    try {
      port.disconnect();
    } catch (_err) {
      /* 已经断开 */
    }
  },
};

function classifyHostError(message) {
  const text = String(message || "");
  if (/no such native application|native messaging host not found|not found|not installed/i.test(text)) {
    return {
      host: "missing",
      detail: "未安装 RDM 原生宿主：请运行 scripts/install-firefox-host.ps1",
    };
  }
  if (/permission|forbidden|allowed_extensions/i.test(text)) {
    return {
      host: "error",
      detail: "扩展 ID 未被宿主 manifest 允许（检查 allowed_extensions）",
    };
  }
  return { host: "error", detail: truncate(text || "原生宿主连接失败", 160) };
}

// ---------------------------------------------------------------------------
// 在线状态 / 徽章
// ---------------------------------------------------------------------------

let statusInFlight = null;
let refreshTimer = null;

function refreshStatus() {
  if (statusInFlight) return statusInFlight;
  statusInFlight = pingHost().finally(() => {
    statusInFlight = null;
  });
  return statusInFlight;
}

function refreshStatusSoon() {
  if (refreshTimer) clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    refreshStatus().catch(() => {});
  }, 800);
}

async function pingHost() {
  try {
    const response = await nativeClient.request({ type: "ping" }, PING_TIMEOUT_MS);
    if (response && response.type === "pong") {
      setStatus("ok", response.gui_running === true, "", response.version || "");
      return { ok: true, guiRunning: response.gui_running === true, hostVersion: response.version || "" };
    }
    setStatus("error", false, "宿主返回了无法识别的 pong 响应");
    return { ok: false, guiRunning: false, error: "unexpected pong" };
  } catch (err) {
    const message = String((err && err.message) || err);
    const classified = classifyHostError(message);
    setStatus(classified.host, false, classified.detail);
    return { ok: false, guiRunning: false, error: message };
  }
}

// ---------------------------------------------------------------------------
// 下载进度（status）
//
// 冻结契约（docs/extension-protocol.md §1.3）：
//   popup → background：{type:"status"}
//   background → popup（成功）：
//     {ok:true, guiRunning, active, globalSpeedBps,
//      downloads:[{id,fileName,state,downloaded,total,speedBps,connections,
//                  activeConnections,targetConnections,peakConnections}]}
//   失败/宿主不可用：
//     {ok:false, guiRunning:false, downloads:[], active:0, globalSpeedBps:0,
//      error, code}
//   code：host_outdated（旧宿主不认 status）| timeout | host_unavailable | error。
//   GUI 未运行不是错误：宿主仍返回 ok + gui_running:false + 空 downloads（无 code）。
// 所有字段类型防御：坏数据丢弃/回退，绝不把异常抛给 popup。
// ---------------------------------------------------------------------------

/** status 失败时允许的 code（popup 根据它显示可操作提示）。 */
const STATUS_FAILURE_CODES = Object.freeze([
  "host_outdated",
  "timeout",
  "host_unavailable",
  "error",
]);

/** status 失败时的固定形状（popup 只读取 ok/downloads/code，其余字段保持类型稳定）。 */
function statusUnavailable(error, code) {
  return {
    ok: false,
    guiRunning: false,
    downloads: [],
    active: 0,
    globalSpeedBps: 0,
    error: typeof error === "string" && error ? error : "status_unavailable",
    code: STATUS_FAILURE_CODES.includes(code) ? code : "error",
  };
}

/** 原生请求异常 → 失败 code：响应超时 / 宿主不可用（缺失、断开、connectNative 失败）。 */
function classifyStatusFailure(message) {
  return /超时|timeout/i.test(String(message || "")) ? "timeout" : "host_unavailable";
}

/**
 * 宿主 error 帧 → 失败 code。
 * 旧版原生宿主不认识 `status`，会回 invalid_request / "unknown message type"；
 * 这是用户最常见的"看不到进度"原因，必须单独标记为 host_outdated。
 */
function classifyStatusErrorFrame(response) {
  const code = typeof response.code === "string" ? response.code : "";
  const message = typeof response.message === "string" ? response.message : "";
  if (code === "invalid_request" || /unknown message type/i.test(message)) return "host_outdated";
  return "error";
}

/** 正的有限数值；字符串/NaN/负数一律回退 fallback。 */
function finitePositiveNumber(value, fallback) {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : fallback;
}

/** 宿主 snake_case 下载项 → camelCase（字段校验交给 RDMPure.normalizeDownloads）。 */
function mapHostDownload(raw) {
  if (!raw || typeof raw !== "object") return null;
  return {
    id: raw.id,
    fileName: raw.file_name,
    state: raw.state,
    downloaded: raw.downloaded,
    total: raw.total === undefined ? null : raw.total,
    speedBps: raw.speed_bps,
    connections: raw.connections,
    // 旧宿主 / 旧 GUI 不返回这三个字段 → normalizeDownloads 统一 0 兜底。
    activeConnections: raw.active_connections,
    targetConnections: raw.target_connections,
    peakConnections: raw.peak_connections,
  };
}

/** 宿主 status 响应 → popup 契约形状（gui_running:false 仍是成功响应）。 */
function normalizeStatusResponse(response) {
  const reply = response && typeof response === "object" ? response : {};
  const rawDownloads = Array.isArray(reply.downloads) ? reply.downloads : [];
  const mapped = [];
  for (const raw of rawDownloads) {
    const item = mapHostDownload(raw);
    if (item) mapped.push(item);
  }
  return {
    ok: true,
    guiRunning: reply.gui_running === true,
    active: Math.trunc(finitePositiveNumber(reply.active, 0)),
    globalSpeedBps: finitePositiveNumber(reply.global_speed_bps, 0),
    downloads: normalizeDownloads(mapped),
  };
}

/** 向宿主查询下载进度；响应/超时/断开/坏数据都归一化为结构化结果，不抛错。 */
async function fetchDownloadStatus() {
  let response;
  try {
    response = await nativeClient.request({ type: "status" }, STATUS_TIMEOUT_MS);
  } catch (err) {
    const message = String((err && err.message) || err || "status_error");
    const classified = classifyHostError(message);
    return statusUnavailable(
      classified.detail || message,
      classifyStatusFailure(message)
    );
  }
  try {
    if (!response || typeof response !== "object") {
      return statusUnavailable("status_unexpected_response", "error");
    }
    if (response.type === "error") {
      const code = typeof response.code === "string" ? response.code : "";
      const message = typeof response.message === "string" ? response.message : "";
      return statusUnavailable(
        [code, message].filter(Boolean).join("：") || "status_error",
        classifyStatusErrorFrame(response)
      );
    }
    if (response.type !== "status") {
      return statusUnavailable("status_unexpected_response", "error");
    }
    if (response.ok === false) {
      return statusUnavailable(
        typeof response.error === "string" ? response.error : "status_failed",
        "error"
      );
    }
    return normalizeStatusResponse(response);
  } catch (err) {
    debug("归一化 status 响应失败: " + err);
    return statusUnavailable("status_invalid_response", "error");
  }
}

function setBadge(text, color, title) {
  if (!browser.action) return;
  try {
    browser.action.setBadgeText({ text }).catch(() => {});
    browser.action.setBadgeBackgroundColor({ color }).catch(() => {});
    browser.action.setTitle({ title }).catch(() => {});
  } catch (err) {
    debug("设置徽章失败: " + err);
  }
}

function updateBadge() {
  const settings = cachedSettings || RDMPure.DEFAULT_SETTINGS;
  if (!settings.enabled) {
    setBadge("off", "#6e7781", "RDM 下载接管：已关闭");
    return;
  }
  if (status.host === "ok" && status.gui) {
    setBadge("ON", "#1f883d", "RDM 在线：下载将交给 RDM（点击查看）");
  } else if (status.host === "ok") {
    setBadge("Q", "#bf8700", "RDM GUI 未运行：下载会先排队（点击查看）");
  } else if (status.host === "missing") {
    setBadge("!", "#cf222e", "未安装 RDM 原生宿主（点击查看安装方法）");
  } else if (status.host === "error") {
    setBadge("!", "#cf222e", "RDM 连接异常（点击查看）");
  } else {
    setBadge("?", "#6e7781", "RDM 状态未知（点击检测）");
  }
}

let flashTimer = null;

function flashBadge(text) {
  if (flashTimer) clearTimeout(flashTimer);
  setBadge(text, text === "✓" ? "#1f883d" : "#bf8700", "RDM 下载接管");
  flashTimer = setTimeout(() => {
    flashTimer = null;
    updateBadge();
  }, 2500);
}

async function ensureAlarm() {
  try {
    const existing = await browser.alarms.get(PING_ALARM);
    if (!existing) {
      // 5 分钟一次足够让徽章状态不至于长期失真；popup 打开/下载转交时
      // 都会立即刷新，避免频繁唤醒事件页和拉起原生宿主。
      await browser.alarms.create(PING_ALARM, { periodInMinutes: 5 });
    }
  } catch (err) {
    debug("创建状态轮询 alarm 失败: " + err);
  }
}

// ---------------------------------------------------------------------------
// webRequest 请求/响应头缓存（MV3 只能观察）
// ---------------------------------------------------------------------------

/** url -> { url, documentUrl, method, headers, response, at } */
const requestCache = new Map();

const WANTED_REQUEST_HEADERS = new Set([
  "user-agent",
  "referer",
  "accept",
  "accept-language",
  "cookie",
]);

const WANTED_RESPONSE_HEADERS = new Set([
  "content-length",
  "content-type",
  "content-disposition",
  "etag",
  "last-modified",
]);

function cacheKey(url) {
  try {
    const parsed = new URL(url);
    parsed.hash = "";
    return parsed.toString();
  } catch (_err) {
    return String(url);
  }
}

function pruneRequestCache() {
  const now = Date.now();
  for (const [key, entry] of requestCache) {
    if (now - entry.at > CACHE_TTL_MS) requestCache.delete(key);
  }
  while (requestCache.size > MAX_CACHE_ENTRIES) {
    const oldestKey = requestCache.keys().next().value;
    requestCache.delete(oldestKey);
  }
}

function rememberRequest(details) {
  if (!isHttpUrl(details.url)) return;
  pruneRequestCache();
  const entry = {
    url: details.url,
    documentUrl: typeof details.documentUrl === "string" ? details.documentUrl : "",
    method: typeof details.method === "string" ? details.method : "GET",
    headers: {},
    response: {},
    at: Date.now(),
  };
  requestCache.set(cacheKey(details.url), entry);
}

function lookupRequest(url) {
  if (!url) return null;
  pruneRequestCache();
  return requestCache.get(cacheKey(url)) || null;
}

function collectHeaders(headers, wanted) {
  const out = {};
  if (!Array.isArray(headers)) return out;
  for (const header of headers) {
    if (!header || typeof header.name !== "string" || typeof header.value !== "string") continue;
    const name = header.name.toLowerCase();
    if (!wanted.has(name)) continue;
    if (/[\r\n\u0000]/.test(header.value)) continue;
    out[name] = header.value;
  }
  return out;
}

// ---------------------------------------------------------------------------
// 接管决策与转交
// ---------------------------------------------------------------------------

/** 我们主动回退浏览器下载的 URL -> 过期时间，用于防止 onCreated 再次接管。 */
const fallbackGuard = new Map();
/** 正在处理的下载 id，防止重复进入。 */
const pendingDecisions = new Set();

function guardFallback(url) {
  if (!isHttpUrl(url)) return;
  const now = Date.now();
  fallbackGuard.set(cacheKey(url), now + FALLBACK_GUARD_TTL_MS);
  for (const [key, expiry] of fallbackGuard) {
    if (expiry <= now) fallbackGuard.delete(key);
  }
  while (fallbackGuard.size > FALLBACK_GUARD_MAX) {
    const oldest = fallbackGuard.keys().next().value;
    fallbackGuard.delete(oldest);
  }
}

function consumeFallbackGuard(url) {
  if (!url) return false;
  const key = cacheKey(url);
  const expiry = fallbackGuard.get(key);
  if (expiry === undefined) return false;
  fallbackGuard.delete(key);
  return expiry > Date.now();
}

function firstHttpUrl(candidates) {
  for (const candidate of candidates) {
    if (isHttpUrl(candidate)) return candidate;
  }
  return "";
}

function cachedResponseHeader(cached, name) {
  if (!cached || !cached.response) return "";
  const value = cached.response[name];
  return typeof value === "string" ? value : "";
}

function resolveFileName(item, cached, url) {
  const fromDisposition = parseContentDispositionFilename(
    cachedResponseHeader(cached, "content-disposition")
  );
  if (fromDisposition) return fromDisposition;
  const fromItem = sanitizeFileName(String(item.filename || "").split(/[\\/]/).pop() || "");
  if (fromItem) return fromItem;
  return fileNameFromUrl(url);
}

function resolveSize(item, cached) {
  const lengthHeader = cachedResponseHeader(cached, "content-length");
  if (lengthHeader) {
    const parsed = parseInt(lengthHeader, 10);
    if (Number.isFinite(parsed) && parsed >= 0) return parsed;
  }
  if (typeof item.totalBytes === "number" && item.totalBytes > 0) return item.totalBytes;
  return -1;
}

async function readCookies(url, observedCookieHeader) {
  if (
    typeof observedCookieHeader === "string" &&
    observedCookieHeader &&
    !/[\r\n\u0000]/.test(observedCookieHeader) &&
    observedCookieHeader.length <= 16 * 1024
  ) {
    return observedCookieHeader;
  }
  try {
    const cookies = await browser.cookies.getAll({ url });
    return buildCookieHeader(cookies);
  } catch (err) {
    debug("读取 Cookie 失败: " + err);
    return "";
  }
}

/**
 * 构造发给原生宿主的 request 对象（docs/extension-protocol.md §1.3）。
 * 只发送已定义字段，避免 undefined 在 native messaging 序列化时报错。
 */
async function buildRequest(options) {
  const settings = options.settings;
  const url = options.url;
  const cached = options.cached || null;
  const cachedHeaders = (cached && cached.headers) || {};
  // 不发送 connections：RDM 桌面端「设置 → 下载 → 默认连接数」是唯一权威，
  // GUI 会把浏览器转交任务的连接数覆盖成自己的设置。
  const request = {
    url,
    source: "firefox",
  };

  const fileName = sanitizeFileName(options.fileName || "");
  if (fileName) request.file_name = fileName;
  if (settings.outputDir) request.output_dir = settings.outputDir;

  const headers = {};
  if (cachedHeaders["accept"]) headers["Accept"] = cachedHeaders["accept"];
  if (cachedHeaders["accept-language"]) {
    headers["Accept-Language"] = cachedHeaders["accept-language"];
  }
  if (Object.keys(headers).length > 0) request.headers = headers;

  const userAgent =
    cachedHeaders["user-agent"] ||
    (typeof navigator !== "undefined" && navigator.userAgent ? navigator.userAgent : "");
  if (userAgent) request.user_agent = userAgent;

  const referer = firstHttpUrl([cachedHeaders["referer"], cached && cached.documentUrl]);
  if (referer) request.referer = referer;

  if (settings.speedLimitKiBps > 0) {
    request.speed_limit_bps = settings.speedLimitKiBps * 1024;
  }

  const sourcePage = firstHttpUrl([
    options.sourcePage,
    cached && cached.documentUrl,
    cachedHeaders["referer"],
  ]);
  if (sourcePage) request.source_page = sourcePage;

  if (settings.forwardCookies) {
    const cookie = await readCookies(url, cachedHeaders["cookie"]);
    if (cookie) request.cookie = cookie;
  }
  return request;
}

async function sendHandoff(request, settings) {
  return nativeClient.request(
    {
      type: "enqueue",
      launch_gui: settings.autoStartGui,
      request,
    },
    ENQUEUE_TIMEOUT_MS
  );
}

// ---------------------------------------------------------------------------
// AI 解析（ai_parse）
// ---------------------------------------------------------------------------

/** 当前窗口的活动标签页（拿不到时返回 null，调用方给出可读错误）。 */
async function getActiveTab() {
  try {
    const tabs = await browser.tabs.query({ active: true, currentWindow: true });
    return Array.isArray(tabs) && tabs.length > 0 ? tabs[0] : null;
  } catch (err) {
    debug("读取当前标签页失败: " + err);
    return null;
  }
}

/** referer 只取 webRequest 观察到的 http(s) 值；Cookie 永远不进入日志。 */
function aiReferer(url) {
  const cached = lookupRequest(url);
  return firstHttpUrl([cached && cached.headers && cached.headers["referer"]]);
}

/** 两个 URL 是否指向同一页面（忽略 fragment；query/端口参与比较）。 */
function samePageUrl(left, right) {
  try {
    const a = new URL(String(left));
    const b = new URL(String(right));
    a.hash = "";
    b.hash = "";
    return a.href === b.href;
  } catch (_err) {
    return false;
  }
}

/**
 * 采集当前标签页的页面链接快照（v0.3，仅随「附带 Cookie 凭证」同意一起采集）。
 *
 * 通过 `browser.scripting.executeScript` 在主 frame 内读取 `location.href`、
 * `document.title` 与 `a[href]` 绝对 URL；不读取 HTML/body/表单/JS 状态。
 * 任何失败（无权限、页面受限、API 缺失）都返回 null —— 解析本身继续。
 * 日志只记链接数量，绝不记录链接值/标题。
 */
async function collectPageSnapshot(tabId) {
  if (typeof tabId !== "number" || tabId < 0) return null;
  try {
    const results = await browser.scripting.executeScript({
      target: { tabId, allFrames: false },
      func: () => ({
        url: location.href,
        title: document.title,
        links: Array.from(document.querySelectorAll("a[href]"))
          .map((anchor) => anchor.href)
          .filter((href) => typeof href === "string" && href.length > 0),
      }),
    });
    const first = Array.isArray(results) && results.length > 0 ? results[0] : null;
    const snapshot = first && typeof first === "object" ? first.result : null;
    if (!snapshot || typeof snapshot !== "object") return null;
    const links = Array.isArray(snapshot.links) ? snapshot.links : [];
    debug("页面链接快照: links=" + links.length);
    return {
      url: typeof snapshot.url === "string" ? snapshot.url : "",
      title: typeof snapshot.title === "string" ? snapshot.title : "",
      links,
    };
  } catch (err) {
    debug(
      "采集页面链接快照失败（不阻塞解析）: " +
        truncate(err && err.message ? err.message : err, 160)
    );
    return null;
  }
}

/**
 * 组装 ai_parse 消息：只有用户勾选「附带 Cookie 凭证」时才读取并携带 Cookie，
 * 并在同一授权下采集页面链接快照。Cookie/快照失败都不阻塞解析。
 */
async function buildAiMessage(options) {
  const settings = options.settings;
  const url = firstHttpUrl([options.url]);
  if (!url) return null;
  const includeCredentials = resolveAiIncludeCredentials(
    settings,
    options.includeCredentials
  );
  let cookie = "";
  if (includeCredentials) {
    try {
      const cookies = await browser.cookies.getAll({ url });
      cookie = buildCookieHeader(cookies);
    } catch (err) {
      debug("读取 AI 解析 Cookie 失败: " + err);
    }
  }
  let snapshot = null;
  if (includeCredentials) {
    try {
      let tabId = Number.isInteger(options.tabId) ? options.tabId : null;
      let tabUrl = firstHttpUrl([options.tabUrl]);
      if (tabId === null) {
        const tab = await getActiveTab();
        if (tab && Number.isInteger(tab.id)) {
          tabId = tab.id;
          tabUrl = firstHttpUrl([tab.url]);
        }
      }
      // 只采集「同一页面」的快照：标签页 URL 必须与要解析的 URL 一致。
      if (tabId !== null && (!tabUrl || samePageUrl(tabUrl, url))) {
        snapshot = await collectPageSnapshot(tabId);
      }
    } catch (err) {
      debug("页面快照初始化失败（不阻塞解析）: " + truncate(err, 160));
    }
  }
  if (snapshot && snapshot.url && !samePageUrl(snapshot.url, url)) {
    debug("页面快照与目标页面不一致，已忽略");
    snapshot = null;
  }
  return buildAiParseRequest({
    url,
    title: options.title,
    sourcePage: firstHttpUrl([options.sourcePage, url]),
    includeCredentials,
    cookie,
    referer: aiReferer(url),
    userAgent:
      typeof navigator !== "undefined" && navigator.userAgent ? navigator.userAgent : "",
    launchGui: settings.autoStartGui,
    pageUrl: snapshot ? snapshot.url : "",
    pageTitle: snapshot ? snapshot.title : "",
    pageLinks: snapshot ? snapshot.links : [],
  });
}

async function sendAiParse(message) {
  return nativeClient.request(message, AI_PARSE_TIMEOUT_MS);
}

/**
 * popup 消息 / 右键菜单共用的 AI 解析入口。
 * @returns {Promise<{ok: boolean, message: string, code?: string, requestId?: number}>}
 */
async function handleAiParseRequest(request) {
  const settings = await loadSettings();
  let url = firstHttpUrl([request && request.url]);
  let title = typeof (request && request.title) === "string" ? request.title : "";
  if (!url) {
    const tab = await getActiveTab();
    if (!tab) {
      return {
        ok: false,
        code: "no_tab",
        message: "AI 解析未发送：没有可解析的当前标签页",
      };
    }
    url = firstHttpUrl([tab.url]);
    title = title || (typeof tab.title === "string" ? tab.title : "");
    if (!url) {
      return {
        ok: false,
        code: "non_http",
        message: "AI 解析未发送：当前页面不是 http(s) 页面",
      };
    }
  }

  const message = await buildAiMessage({
    url,
    title,
    settings,
    includeCredentials: request ? request.includeCredentials : undefined,
    tabId: request ? request.tabId : undefined,
    tabUrl: request ? request.tabUrl : undefined,
  });
  if (!message) {
    return { ok: false, code: "invalid_url", message: "AI 解析未发送：页面地址无效" };
  }

  // 只记录脱敏 URL 与是否带凭据；绝不打 Cookie 值。
  debug(
    "AI 解析请求: url=" +
      redactUrl(message.url) +
      " credentials=" +
      (message.include_credentials === true)
  );

  let response;
  try {
    response = await sendAiParse(message);
  } catch (err) {
    const text = String((err && err.message) || err);
    const classified = classifyHostError(text);
    setStatus(classified.host, false, classified.detail);
    return {
      ok: false,
      code: "host_error",
      message: "AI 解析未发送：" + truncate(classified.detail || text, 160),
    };
  }

  const result = formatAiParseResult(response);
  if (result.ok) {
    setStatus("ok", true, "");
    flashBadge("✓");
  } else {
    flashBadge("!");
  }
  return result;
}

/** 右键菜单「用 RDM AI 解析此页」：解析当前标签页并写入最近记录。 */
async function handleContextMenuAiParse(info, tab) {
  const pageUrl = firstHttpUrl([tab && tab.url]);
  if (!pageUrl) {
    flashBadge("!");
    return;
  }
  const pageTitle = tab && typeof tab.title === "string" ? tab.title : "";
  // 快照采集需要标签页 id（buildAiMessage 用它定位当前页面）。
  const result = await handleAiParseRequest({
    url: pageUrl,
    title: pageTitle,
    tabId: tab && Number.isInteger(tab.id) ? tab.id : undefined,
    tabUrl: pageUrl,
  });
  await pushRecent(aiParseRecentEntry({ url: pageUrl, title: pageTitle }, result));
}

function recentEntryFromResponse(request, response) {
  const handed = response.type === "enqueued";
  const queuedDetail = response.credentials_stripped
    ? "RDM 未运行，任务已排队，但登录态/授权信息不会保存；如需要登录态请先启动 RDM 后重新下载"
    : "RDM GUI 未运行，已排队，下次启动自动加入";
  return {
    fileName: request.file_name || fileNameFromUrl(request.url),
    url: redactUrl(request.url),
    status: handed ? "handed" : "queued",
    // 只有 handed（GUI 在线、已入队）才有 download_id；queued 落盘无 id，用 "" 表示。
    downloadId:
      handed && typeof response.download_id === "string" ? response.download_id : "",
    detail: handed
      ? "已交给 RDM" + (response.download_id ? "（" + response.download_id + "）" : "")
      : queuedDetail,
  };
}

/**
 * 取消并抹掉浏览器原生下载。
 *
 * 先 cancel；如果下载已经完成（小文件竞态），用 removeFile 删掉文件再 erase。
 * erase 只删历史记录，不删文件，所以顺序不能反。
 */
async function cancelAndEraseBrowserDownload(item) {
  const id = item.id;
  let state = "";
  try {
    await browser.downloads.cancel(id);
  } catch (err) {
    debug("取消下载失败（可能已完成）: " + err);
  }

  const deadline = Date.now() + 1500;
  while (Date.now() < deadline) {
    try {
      const found = await browser.downloads.search({ id });
      state = found && found[0] ? found[0].state : "";
    } catch (_err) {
      state = "";
    }
    if (state !== "in_progress") break;
    await sleep(60);
  }

  if (state === "complete") {
    try {
      await browser.downloads.removeFile(id);
    } catch (err) {
      debug("删除已完成文件失败: " + err);
    }
  }

  try {
    await browser.downloads.erase({ id });
  } catch (err) {
    debug("抹除下载记录失败: " + err);
  }
}

async function fallbackToBrowser(request, reason) {
  const url = request.url;
  guardFallback(url);
  const cached = lookupRequest(url);
  if (cached && cached.redirectUrl) guardFallback(cached.redirectUrl);
  try {
    await browser.downloads.download({
      url,
      conflictAction: "uniquify",
      saveAs: false,
    });
    await pushRecent({
      fileName: request.file_name || fileNameFromUrl(url),
      url: redactUrl(url),
      status: "fallback",
      detail: "转交失败，已回退浏览器下载：" + truncate(reason, 120),
    });
    flashBadge("!");
  } catch (err) {
    await pushRecent({
      fileName: request.file_name || fileNameFromUrl(url),
      url: redactUrl(url),
      status: "error",
      detail: "转交与回退均失败：" + truncate(String(reason) + " / " + err, 140),
    });
    flashBadge("!");
  }
}

async function handleDownloadCreated(item) {
  if (!item || typeof item.id !== "number") return;
  if (pendingDecisions.has(item.id)) return;

  // 我们自己发起（或宿主回退）的下载不再接管。
  if (typeof item.byExtensionId === "string" && item.byExtensionId === browser.runtime.id) {
    return;
  }

  const url = typeof item.url === "string" ? item.url : "";
  if (consumeFallbackGuard(url) || consumeFallbackGuard(item.finalUrl)) {
    debug("检测到回退下载，跳过接管: " + redactUrl(url || item.finalUrl));
    return;
  }

  const settings = await loadSettings();
  const cached = lookupRequest(url) || lookupRequest(item.finalUrl);
  const fileName = resolveFileName(item, cached, url);
  const size = resolveSize(item, cached);
  const decision = shouldIntercept({
    url,
    fileName,
    sizeBytes: size,
    settings,
  });
  if (!decision.intercept) {
    if (decision.reason !== "disabled") {
      debug(
        "跳过接管（" +
          decision.reason +
          "）：" +
          redactUrl(url) +
          " 大小=" +
          formatBytes(size)
      );
    }
    return;
  }

  pendingDecisions.add(item.id);
  try {
    const request = await buildRequest({
      url,
      fileName,
      cached,
      settings,
    });

    // 顺序按协议要求：先取消浏览器下载，再转交原生宿主；
    // 转交失败时 fallbackToBrowser 会重新发起浏览器下载。
    await cancelAndEraseBrowserDownload(item);

    let response = null;
    let failure = "";
    try {
      response = await sendHandoff(request, settings);
    } catch (err) {
      failure = String((err && err.message) || err);
      const classified = classifyHostError(failure);
      setStatus(classified.host, false, classified.detail);
    }

    if (response && (response.type === "enqueued" || response.type === "queued")) {
      setStatus("ok", response.gui_running === true, "");
      await pushRecent(recentEntryFromResponse(request, response));
      flashBadge(response.type === "enqueued" ? "✓" : "Q");
      refreshStatusSoon();
      return;
    }
    if (response && response.type === "error") {
      failure = response.code + "：" + response.message;
    }
    await fallbackToBrowser(request, failure || "原生宿主不可用");
  } catch (err) {
    // 兜底：任何意外都不能让"已取消的浏览器下载"凭空消失。
    const request = {
      url,
      file_name: fileName,
    };
    await fallbackToBrowser(request, "内部错误：" + truncate(String(err), 100));
  } finally {
    pendingDecisions.delete(item.id);
  }
}

// ---------------------------------------------------------------------------
// 右键菜单
// ---------------------------------------------------------------------------

async function ensureContextMenu(settings) {
  const current = settings || (await loadSettings());
  try {
    await browser.contextMenus.remove(MENU_ID);
  } catch (_err) {
    /* 不存在 */
  }
  try {
    await browser.contextMenus.remove(AI_MENU_ID);
  } catch (_err) {
    /* 不存在 */
  }
  if (!current.contextMenu) return;
  try {
    await browser.contextMenus.create({
      id: MENU_ID,
      title: "使用 RDM 下载",
      contexts: ["link", "image", "audio", "video", "selection"],
    });
    await browser.contextMenus.create({
      id: AI_MENU_ID,
      title: "用 RDM AI 解析此页",
      contexts: ["page", "link"],
    });
  } catch (err) {
    debug("创建右键菜单失败: " + err);
  }
}

async function handleContextMenuClick(info, tab) {
  const settings = await loadSettings();
  const selection = typeof info.selectionText === "string" ? info.selectionText.trim() : "";
  const url = firstHttpUrl([info.linkUrl, info.srcUrl, selection]);
  if (!url) {
    flashBadge("!");
    return;
  }
  const cached = lookupRequest(url);
  const pageUrl = firstHttpUrl([tab && tab.url, cached && cached.documentUrl]);
  try {
    const request = await buildRequest({
      url,
      fileName: resolveContextFileName(url, cached),
      cached,
      settings,
      sourcePage: pageUrl,
    });
    const response = await sendHandoff(request, settings);
    if (response && (response.type === "enqueued" || response.type === "queued")) {
      setStatus("ok", response.gui_running === true, "");
      await pushRecent(recentEntryFromResponse(request, response));
      flashBadge(response.type === "enqueued" ? "✓" : "Q");
    } else {
      const detail =
        response && response.type === "error"
          ? response.code + "：" + response.message
          : "宿主响应异常";
      await pushRecent({
        fileName: request.file_name || fileNameFromUrl(url),
        url: redactUrl(url),
        status: "error",
        detail: truncate(detail, 140),
      });
      flashBadge("!");
    }
  } catch (err) {
    const message = String((err && err.message) || err);
    const classified = classifyHostError(message);
    setStatus(classified.host, false, classified.detail);
    await pushRecent({
      fileName: fileNameFromUrl(url),
      url: redactUrl(url),
      status: "error",
      detail: truncate(message, 140),
    });
    flashBadge("!");
  }
}

function resolveContextFileName(url, cached) {
  const fromDisposition = parseContentDispositionFilename(
    cachedResponseHeader(cached, "content-disposition")
  );
  if (fromDisposition) return fromDisposition;
  return fileNameFromUrl(url);
}

// ---------------------------------------------------------------------------
// popup / options 消息接口
// ---------------------------------------------------------------------------

async function getState() {
  const settings = await loadSettings();
  const recentList = (await loadRecent()).slice(0, 8);
  return {
    ok: true,
    settings,
    status,
    recent: recentList,
    debug: debugLines.slice(-10),
    extensionVersion: browser.runtime.getManifest().version,
    nativeHost: HOST_NAME,
  };
}

browser.runtime.onMessage.addListener((message) => {
  if (!message || typeof message !== "object" || typeof message.type !== "string") {
    return Promise.resolve({ ok: false, error: "invalid_message" });
  }
  switch (message.type) {
    case "getState":
      return getState();
    case "testConnection":
      return refreshStatus().then(() => getState());
    case "setSettings":
      return saveSettings(message.settings).then(() => getState());
    case "clearRecent":
      return clearRecent().then(() => getState());
    case "status":
      return fetchDownloadStatus();
    case "aiParse":
      return handleAiParseRequest(message);
    case "openOptions":
      return browser.runtime.openOptionsPage().then(() => ({ ok: true }));
    default:
      return Promise.resolve({ ok: false, error: "unknown_message_type" });
  }
});

// ---------------------------------------------------------------------------
// 事件监听（必须在顶层同步注册）
// ---------------------------------------------------------------------------

browser.webRequest.onBeforeRequest.addListener(
  (details) => {
    rememberRequest(details);
  },
  { urls: ["http://*/*", "https://*/*"] }
);

browser.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    const entry = lookupRequest(details.url);
    if (!entry) return;
    entry.at = Date.now();
    Object.assign(
      entry.headers,
      collectHeaders(details.requestHeaders, WANTED_REQUEST_HEADERS)
    );
  },
  { urls: ["http://*/*", "https://*/*"] },
  ["requestHeaders"]
);

browser.webRequest.onHeadersReceived.addListener(
  (details) => {
    const entry = lookupRequest(details.url);
    if (!entry) return;
    const responseHeaders = collectHeaders(details.responseHeaders, WANTED_RESPONSE_HEADERS);
    entry.at = Date.now();
    entry.response = Object.assign({}, entry.response, responseHeaders);
  },
  { urls: ["http://*/*", "https://*/*"] },
  ["responseHeaders"]
);

browser.webRequest.onBeforeRedirect.addListener(
  (details) => {
    const entry = lookupRequest(details.url);
    if (!entry || !isHttpUrl(details.redirectUrl)) return;
    entry.redirectUrl = details.redirectUrl;
    entry.at = Date.now();
    requestCache.set(cacheKey(details.redirectUrl), entry);
  },
  { urls: ["http://*/*", "https://*/*"] }
);

browser.downloads.onCreated.addListener((item) => {
  handleDownloadCreated(item).catch((err) => {
    debug("处理下载事件失败: " + err);
  });
});

browser.contextMenus.onClicked.addListener((info, tab) => {
  if (!info || typeof info.menuItemId !== "string") return;
  if (info.menuItemId === AI_MENU_ID) {
    handleContextMenuAiParse(info, tab).catch((err) => {
      debug("右键 AI 解析失败: " + err);
    });
    return;
  }
  if (info.menuItemId !== MENU_ID) return;
  handleContextMenuClick(info, tab).catch((err) => {
    debug("右键转交失败: " + err);
  });
});

browser.alarms.onAlarm.addListener((alarm) => {
  if (!alarm || alarm.name !== PING_ALARM) return;
  refreshStatus().catch(() => {});
});

browser.runtime.onInstalled.addListener(() => {
  initialize()
    .then(() => ensureContextMenu())
    .catch((err) => debug("初始化失败: " + err));
});

browser.runtime.onStartup.addListener(() => {
  initialize()
    .then(() => ensureContextMenu())
    .catch((err) => debug("初始化失败: " + err));
});

browser.storage.onChanged.addListener((changes, areaName) => {
  if (areaName !== "local" || !changes[SETTINGS_KEY]) return;
  cachedSettings = normalizeSettings(changes[SETTINGS_KEY].newValue);
  updateBadge();
});

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

const STATUS_MAX_AGE_MS = 5 * 60 * 1000;

async function initialize() {
  await loadSettings();
  status = await loadStoredStatus();
  updateBadge();
  await ensureAlarm();
  // 事件页会被 webRequest 事件频繁唤醒；只有状态过期/未知时才拉起原生宿主，
  // 避免普通浏览时反复启动宿主进程。
  const stale = status.host === "unknown" || Date.now() - status.checkedAt > STATUS_MAX_AGE_MS;
  if (stale) {
    refreshStatus().catch(() => {});
  }
}

void initialize().catch((err) => debug("后台初始化失败: " + err));
