/*!
 * RDM 下载接管 — Firefox 后台事件页（Manifest V3）
 *
 * 职责：
 *   1. 用 webRequest 观察 http(s) 请求/响应头（MV3 下只能观察，不能 block）；
 *   2. 监听 downloads.onCreated，按设置决定是否接管：
 *        原生宿主可用 → 取消并抹掉浏览器下载 → native messaging 转交 RDM；
 *        宿主不可用/转交失败 → 重新发起浏览器下载（并防止再次拦截形成死循环）；
 *   3. 维护原生宿主 / GUI 在线状态与工具栏徽章；
 *   4. 右键菜单「使用 RDM 下载」；
 *   5. 为 popup / options 提供消息接口。
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
} = RDMPure;

const HOST_NAME = "com.rdm.download";
const SETTINGS_KEY = "rdm.settings";
const RECENT_KEY = "rdm.recent";
const STATUS_KEY = "rdm.status";
const PING_ALARM = "rdm.status.ping";
const MENU_ID = "rdm-download-with";
const MAX_RECENT = 20;
const PING_TIMEOUT_MS = 3000;
/** GUI 未被启动时，宿主最多轮询 8 秒，这里留足余量。 */
const ENQUEUE_TIMEOUT_MS = 16000;
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

async function loadRecent() {
  if (recent) return recent;
  const stored = await storageGet(RECENT_KEY);
  recent = Array.isArray(stored[RECENT_KEY]) ? stored[RECENT_KEY] : [];
  return recent;
}

async function pushRecent(entry) {
  const list = (await loadRecent()).slice();
  list.unshift(Object.assign({ at: Date.now() }, entry));
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
  const request = {
    url,
    connections: settings.connections,
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

function recentEntryFromResponse(request, response) {
  const handed = response.type === "enqueued";
  const queuedDetail = response.credentials_stripped
    ? "RDM 未运行，任务已排队，但登录态/授权信息不会保存；如需要登录态请先启动 RDM 后重新下载"
    : "RDM GUI 未运行，已排队，下次启动自动加入";
  return {
    fileName: request.file_name || fileNameFromUrl(request.url),
    url: redactUrl(request.url),
    status: handed ? "handed" : "queued",
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
  if (!current.contextMenu) return;
  try {
    await browser.contextMenus.create({
      id: MENU_ID,
      title: "使用 RDM 下载",
      contexts: ["link", "image", "audio", "video", "selection"],
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
  if (!info || info.menuItemId !== MENU_ID) return;
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
