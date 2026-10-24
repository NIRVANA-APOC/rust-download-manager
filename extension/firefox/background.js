/*!
 * RDM 下载接管 — Firefox 后台事件页（Manifest V3）
 *
 * 职责：
 *   1. 用 webRequest 观察 http(s) 请求/响应头（MV3 下只能观察，不能 block）；
 *   2. 监听 downloads.onCreated，按设置决定是否接管：
 *        原生宿主可用 → 取消并抹掉浏览器下载 → native messaging 转交 RDM；
 *        宿主不可用/转交失败 → 重新发起浏览器下载（并防止再次拦截形成死循环）；
 *   3. 维护原生宿主 / GUI 在线状态与工具栏徽章；
 *   4. 右键菜单「使用 RDM 下载链接 / 解析页面资源…」；
 *   5. 为 popup / options 提供消息接口（含解析页面资源 ai_parse）。
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
  sanitizePageLinks,
  parseUrlsFromText,
  summarizeActivity,
  computeBadge,
  formatHandoffResult,
} = RDMPure;

const HOST_NAME = "com.rdm.download";
const SETTINGS_KEY = "rdm.settings";
const RECENT_KEY = "rdm.recent";
const STATUS_KEY = "rdm.status";
const ACTIVITY_KEY = "rdm.activity";
const PING_ALARM = "rdm.status.ping";
const ACTIVITY_ALARM = "rdm.activity.poll";
const MENU_LINK_ID = "rdm-download-with";
const MENU_IMAGE_ID = "rdm-download-image";
const MENU_MEDIA_ID = "rdm-download-media";
const MENU_PAGE_ID = "rdm-parse-page";
const MENU_SELECTION_ID = "rdm-download-selection";
const MENU_OPTIONS_ID = "rdm-options";
const MENU_SEPARATOR_ID = "rdm-menu-separator";
const MAX_RECENT = 20;
/** popup 只展示 5 条最近活动；background 仍然保留 20 条供桌面端/历史使用。 */
const POPUP_RECENT = 5;
const PING_TIMEOUT_MS = 3000;
/** 「打开 RDM」：v0.3 宿主已支持 activate（含冷启动拉起，最多约 8s），
 *  留 9.5s 超时；旧宿主回 unknown message type 时由调用方降级提示。 */
const ACTIVATE_TIMEOUT_MS = 9500;
/** popup 主按钮批量转交最多 URL 数（07 §4.2：选中多个链接时批量转交）。 */
const MAX_HANDOFF_URLS = 10;
/** 页面选中文本最多读取字符数（仅本地解析，不发送原始文本）。 */
const MAX_SELECTION_CHARS = 8000;
/** popup 每秒轮询一次下载进度；宿主/GUI 正常时应远快于该预算。 */
const STATUS_TIMEOUT_MS = 3000;
/** GUI 未被启动时，宿主最多轮询 8 秒，这里留足余量。 */
const ENQUEUE_TIMEOUT_MS = 16000;
/** 解析页面资源同样可能触发 GUI 启动，给与 enqueue 相同的预算。 */
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

/** 「最近活动」允许的状态；未知值按 error 处理（保持旧行为）。 */
const RECENT_STATES = ["handed", "queued", "duplicate", "fallback", "ai", "error"];

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

/** 清空后暂存的列表，供 popup 撤销（07 §7：restoreRecent，仅内存不落盘）。 */
let lastClearedRecent = null;

async function clearRecent() {
  const list = (await loadRecent()).slice();
  lastClearedRecent = list.slice();
  recent = [];
  await storageSet({ [RECENT_KEY]: [] });
}

/** 撤销「清空最近活动」：把清空前内存里的列表合并回来（去重、限 20 条）。 */
async function restoreRecent() {
  if (!Array.isArray(lastClearedRecent) || lastClearedRecent.length === 0) return false;
  const current = (await loadRecent()).slice();
  const merged = [];
  const seen = new Set();
  for (const entry of lastClearedRecent.concat(current)) {
    const key = String(entry.at || 0) + "|" + entry.url + "|" + entry.fileName + "|" + entry.status;
    if (seen.has(key)) continue;
    seen.add(key);
    merged.push(normalizeRecentEntry(entry));
  }
  recent = merged.slice(0, MAX_RECENT);
  lastClearedRecent = null;
  await storageSet({ [RECENT_KEY]: recent });
  return true;
}

// ---------------------------------------------------------------------------
// 活动下载缓存（badge 用；07 §8）
// ---------------------------------------------------------------------------

/** 最近一次 status 快照的活动汇总（持久化，启动后 badge 不必等网络）。 */
let cachedActivity = { downloading: 0, paused: 0, failed: 0, active: 0, speedBps: 0, updatedAt: 0 };

function normalizeStoredActivity(raw) {
  const input = raw && typeof raw === "object" ? raw : {};
  const clamp = (value) => {
    const parsed = typeof value === "number" ? value : parseInt(value, 10);
    return Number.isFinite(parsed) && parsed > 0 ? Math.trunc(parsed) : 0;
  };
  return {
    downloading: clamp(input.downloading),
    paused: clamp(input.paused),
    failed: clamp(input.failed),
    active: clamp(input.active),
    speedBps: Number.isFinite(input.speedBps) && input.speedBps > 0 ? input.speedBps : 0,
    updatedAt: typeof input.updatedAt === "number" ? input.updatedAt : 0,
  };
}

async function loadStoredActivity() {
  const stored = await storageGet(ACTIVITY_KEY);
  cachedActivity = normalizeStoredActivity(stored[ACTIVITY_KEY]);
  return cachedActivity;
}

/** status 结果 → badge 活动汇总缓存（失败快照不覆盖上一次已知值）。 */
function recordActivity(result) {
  if (!result || result.ok !== true) return cachedActivity;
  const summary = summarizeActivity(normalizeDownloads(result.downloads));
  cachedActivity = Object.assign(summary, { updatedAt: Date.now() });
  updateBadge();
  storageSet({ [ACTIVITY_KEY]: cachedActivity });
  return cachedActivity;
}

let activityRefreshTimer = null;

/** 转交成功后延迟刷新 badge（不阻塞 popup 反馈）。 */
function refreshActivitySoon() {
  if (activityRefreshTimer) clearTimeout(activityRefreshTimer);
  activityRefreshTimer = setTimeout(() => {
    activityRefreshTimer = null;
    fetchDownloadStatus()
      .then((result) => recordActivity(result))
      .catch(() => {});
  }, 800);
}

/** 浏览器类型（popup/options 显示 Chrome 实验性标签、按浏览器给安装命令）。 */
function detectBrowserKind() {
  try {
    const url = browser.runtime.getURL("");
    if (url.indexOf("moz-extension:") === 0) return "firefox";
    if (url.indexOf("chrome-extension:") === 0) return "chrome";
  } catch (_err) {
    /* 测试/旧环境没有 getURL */
  }
  return "unknown";
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
    const textPromise = browser.action.setBadgeText({ text: text || "" });
    if (textPromise && typeof textPromise.catch === "function") textPromise.catch(() => {});
    if (color) {
      const colorPromise = browser.action.setBadgeBackgroundColor({ color });
      if (colorPromise && typeof colorPromise.catch === "function") colorPromise.catch(() => {});
    }
    const titlePromise = browser.action.setTitle({ title });
    if (titlePromise && typeof titlePromise.catch === "function") titlePromise.catch(() => {});
  } catch (err) {
    debug("设置徽章失败: " + err);
  }
}

/**
 * badge（07 §8）：接管关闭 off / 宿主缺失 ! / GUI 未运行 Q /
 * 活动数（暂停橙、失败红、下载中蓝）/ 空闲无角标。
 * 颜色/文案全部由 lib/pure.js 的 computeBadge 决定，便于单测。
 */
function updateBadge() {
  const settings = cachedSettings || RDMPure.DEFAULT_SETTINGS;
  const badge = computeBadge({
    settings,
    status,
    activity: cachedActivity,
  });
  setBadge(badge.text, badge.color || "#6E7781", badge.title);
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
    // badge 活动数轮询（07 §8）：Chrome/Firefox 的 alarms 最小粒度都是 1 分钟，
    // 设计里的 5s/30s 预算在 MV3 下不可用，这里采用保守的 1 分钟兜底；
    // popup 打开期间的 1s 轮询与转交后的延迟刷新负责实时性。
    const activityAlarm = await browser.alarms.get(ACTIVITY_ALARM);
    if (!activityAlarm) {
      await browser.alarms.create(ACTIVITY_ALARM, { periodInMinutes: 1 });
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

async function sendHandoff(request, settings, launchGuiOverride) {
  const launchGui =
    typeof launchGuiOverride === "boolean" ? launchGuiOverride : settings.autoStartGui;
  return nativeClient.request(
    {
      type: "enqueue",
      launch_gui: launchGui,
      request,
    },
    ENQUEUE_TIMEOUT_MS
  );
}

// ---------------------------------------------------------------------------
// 解析页面资源（ai_parse）
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
      debug("读取解析用 Cookie 失败: " + err);
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
    // 用户显式点击「启动 RDM 并解析」时 forceLaunch=true（07 §4.2）。
    launchGui: options.forceLaunch === true ? true : settings.autoStartGui,
    pageUrl: snapshot ? snapshot.url : "",
    pageTitle: snapshot ? snapshot.title : "",
    pageLinks: snapshot ? snapshot.links : [],
  });
}

async function sendAiParse(message) {
  return nativeClient.request(message, AI_PARSE_TIMEOUT_MS);
}

/**
 * popup 消息 / 右键菜单共用的「解析页面资源」入口。
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
        message: "解析未发送：没有可解析的当前标签页",
      };
    }
    url = firstHttpUrl([tab.url]);
    title = title || (typeof tab.title === "string" ? tab.title : "");
    if (!url) {
      return {
        ok: false,
        code: "non_http",
        message: "解析未发送：当前页面不是 http(s) 页面",
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
    forceLaunch: request ? request.forceLaunch === true : false,
  });
  if (!message) {
    return { ok: false, code: "invalid_url", message: "解析未发送：页面地址无效" };
  }

  // 只记录脱敏 URL 与是否带凭据；绝不打 Cookie 值。
  debug(
    "解析请求: url=" +
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
      message: "解析未发送：" + truncate(classified.detail || text, 160),
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

/** 右键菜单「解析页面资源…」：解析当前标签页并写入最近活动。 */
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

// ---------------------------------------------------------------------------
// popup 上下文 / 活动查询 / 主按钮交接（M4）
// ---------------------------------------------------------------------------

/**
 * 读取当前标签页选中的文本并从中提取 http(s) 链接。
 * 只在用户打开 popup 时调用；原始文本不离开浏览器、不写日志。
 */
async function collectSelectedUrls(tabId) {
  if (!Number.isInteger(tabId) || tabId < 0) return [];
  try {
    const results = await browser.scripting.executeScript({
      target: { tabId, allFrames: false },
      func: () => {
        try {
          const selection = window.getSelection ? window.getSelection() : null;
          return selection ? String(selection.toString()).slice(0, 8000) : "";
        } catch (_err) {
          return "";
        }
      },
    });
    const first = Array.isArray(results) && results.length > 0 ? results[0] : null;
    const text = first && typeof first.result === "string" ? first.result : "";
    return parseUrlsFromText(text, MAX_HANDOFF_URLS);
  } catch (err) {
    debug(
      "读取选中文本失败（不阻塞 popup）: " +
        truncate(err && err.message ? err.message : err, 120)
    );
    return [];
  }
}

/**
 * popup 当前 tab 上下文（07 §4.1 / §17 #1）。
 * 含 URL/标题/可读文件名/已知大小/选中链接；不读取页面正文。
 */
async function handleGetTabContext() {
  const tab = await getActiveTab();
  if (!tab) {
    return { ok: false, code: "no_tab", message: "没有可用的当前标签页" };
  }
  const rawUrl = typeof tab.url === "string" ? tab.url : "";
  const cached = isHttpUrl(rawUrl) ? lookupRequest(rawUrl) : null;
  const fileName = cached ? resolveContextFileName(rawUrl, cached) : fileNameFromUrl(rawUrl);
  const sizeBytes = cached ? resolveSize({}, cached) : -1;
  let selectedUrls = [];
  if (isHttpUrl(rawUrl) && Number.isInteger(tab.id)) {
    selectedUrls = await collectSelectedUrls(tab.id);
  }
  return {
    ok: true,
    tabId: Number.isInteger(tab.id) ? tab.id : -1,
    url: rawUrl,
    title: typeof tab.title === "string" ? tab.title : "",
    fileName,
    sizeBytes: sizeBytes >= 0 ? sizeBytes : -1,
    selectedUrls,
    isHttp: isHttpUrl(rawUrl),
  };
}

/** getActivity：status + 最近活动（popup 每秒轮询一次，badge 同步更新）。 */
async function getActivityResult() {
  const result = await fetchDownloadStatus();
  recordActivity(result);
  const recentList = (await loadRecent()).slice(0, POPUP_RECENT);
  return Object.assign({}, result, { recent: recentList });
}

/** 页面内接管提示（07 §10）：默认关闭；仅在用户主动动作且设置开启时注入。 */
function pageToastFunction(text) {
  try {
    let host = document.getElementById("__rdm-handoff-toast");
    if (!host) {
      host = document.createElement("div");
      host.id = "__rdm-handoff-toast";
      host.style.cssText =
        "position:fixed;right:16px;bottom:16px;z-index:2147483647;max-width:320px;" +
        "padding:10px 14px;border-radius:10px;background:rgba(14,17,22,.92);" +
        "color:#e8edf4;font:13px/1.5 'Segoe UI','Microsoft YaHei UI',sans-serif;" +
        "box-shadow:0 4px 16px rgba(0,0,0,.35);pointer-events:none;";
      (document.body || document.documentElement).appendChild(host);
    }
    host.textContent = "✓ " + String(text || "已交给 RDM");
    if (window.__rdmHandoffToastTimer) clearTimeout(window.__rdmHandoffToastTimer);
    window.__rdmHandoffToastTimer = setTimeout(function () {
      if (host.parentNode) host.parentNode.removeChild(host);
    }, 3500);
  } catch (_err) {
    /* 页面不允许注入时静默失败 */
  }
}

async function showPageToast(tabId, text) {
  if (!Number.isInteger(tabId) || tabId < 0) return false;
  try {
    await browser.scripting.executeScript({
      target: { tabId, allFrames: false },
      func: pageToastFunction,
      args: [String(text || "已交给 RDM")],
    });
    return true;
  } catch (err) {
    debug(
      "页面内接管提示注入失败（不阻塞）: " +
        truncate(err && err.message ? err.message : err, 120)
    );
    return false;
  }
}

/**
 * popup 主按钮交接（下载文件 / 选中的链接）。
 * duplicate 语义（07 §4.3）：不创建第二个任务、不回退浏览器下载。
 */
async function handlePopupHandoff(message) {
  const settings = await loadSettings();
  const rawUrls = Array.isArray(message && message.urls)
    ? message.urls
    : [message && message.url];
  const urls = sanitizePageLinks(rawUrls).slice(0, MAX_HANDOFF_URLS);
  if (urls.length === 0) {
    return { ok: false, code: "no_urls", message: "当前页面没有可交给 RDM 的链接" };
  }
  const forceLaunch = !!(message && message.forceLaunch === true);
  const launchGui = forceLaunch ? true : settings.autoStartGui;
  const tab = await getActiveTab();
  const sourcePage = firstHttpUrl([message && message.sourcePage, tab && tab.url]);
  const preferredName =
    typeof (message && message.fileName) === "string"
      ? sanitizeFileName(message.fileName)
      : "";

  const results = [];
  let successCount = 0;
  let duplicateCount = 0;
  let queuedCount = 0;

  for (const url of urls) {
    const cached = lookupRequest(url);
    const fallbackName = cached ? resolveContextFileName(url, cached) : fileNameFromUrl(url);
    const fileName = urls.length === 1 && preferredName ? preferredName : fallbackName;
    let request;
    try {
      request = await buildRequest({
        url,
        fileName,
        cached,
        settings,
        sourcePage,
      });
    } catch (err) {
      const text = String((err && err.message) || err);
      results.push({
        url: redactUrl(url),
        ok: false,
        code: "internal",
        message: truncate(text, 160),
      });
      continue;
    }
    try {
      const response = await sendHandoff(request, settings, launchGui);
      if (response && (response.type === "enqueued" || response.type === "queued")) {
        successCount += 1;
        if (response.type === "queued") queuedCount += 1;
        setStatus(
          "ok",
          response.type === "enqueued" ? true : response.gui_running === true,
          ""
        );
        await pushRecent(recentEntryFromResponse(request, response));
        results.push({ url: redactUrl(url), ok: true, status: response.type });
        continue;
      }
      if (response && response.type === "error" && response.code === "duplicate") {
        successCount += 1;
        duplicateCount += 1;
        setStatus("ok", true, "");
        await pushRecent({
          fileName: request.file_name || fallbackName,
          url: redactUrl(url),
          status: "duplicate",
          detail: "该下载已在 RDM 中，未重复创建任务",
        });
        results.push({ url: redactUrl(url), ok: true, status: "duplicate" });
        continue;
      }
      const detail =
        response && response.type === "error"
          ? [response.code, response.message].filter(Boolean).join("：")
          : "原生宿主响应异常";
      if (response && response.type === "error" && response.code === "gui_unavailable") {
        setStatus("ok", false, "");
      }
      await pushRecent({
        fileName: request.file_name || fallbackName,
        url: redactUrl(url),
        status: "error",
        detail: truncate(detail, 140),
      });
      results.push({
        url: redactUrl(url),
        ok: false,
        code:
          response && response.type === "error" && typeof response.code === "string"
            ? response.code
            : "internal",
        message: truncate(detail, 160),
      });
    } catch (err) {
      const text = String((err && err.message) || err);
      const classified = classifyHostError(text);
      setStatus(classified.host, false, classified.detail);
      await pushRecent({
        fileName: request.file_name || fallbackName,
        url: redactUrl(url),
        status: "error",
        detail: truncate(text, 140),
      });
      results.push({
        url: redactUrl(url),
        ok: false,
        code: "host_unavailable",
        message: truncate(classified.detail || text, 160),
      });
    }
  }

  const ok = successCount > 0;
  const firstFailure = results.find((item) => !item.ok);
  const code = ok ? "" : (firstFailure && firstFailure.code) || "internal";
  let replyMessage;
  if (!ok) {
    replyMessage = formatHandoffResult({ ok: false, code }).text;
  } else if (duplicateCount === results.length) {
    replyMessage = "该下载已在 RDM 中";
  } else if (queuedCount > 0) {
    replyMessage = "已排队，启动 RDM 后自动开始";
  } else {
    replyMessage = urls.length > 1 ? "已交给 RDM（" + successCount + " 个）" : "已交给 RDM";
  }
  if (ok) {
    flashBadge(queuedCount > 0 ? "Q" : "✓");
    refreshStatusSoon();
    refreshActivitySoon();
  }
  return {
    ok,
    code,
    duplicate: duplicateCount > 0 && successCount === duplicateCount,
    queued: queuedCount > 0,
    results,
    message: replyMessage,
  };
}

/**
 * 「打开 RDM」：宿主 v0.3 起支持 `activate`（会显示/激活主窗口，GUI 未运行时可拉起）。
 * 旧宿主回 `unknown message type`；这里保留向前兼容的降级提示。
 */
async function handleOpenRdm() {
  try {
    const response = await nativeClient.request({ type: "activate" }, ACTIVATE_TIMEOUT_MS);
    if (response && response.type === "error") {
      const unsupported = response.code === "invalid_request";
      return {
        ok: false,
        code: unsupported ? "activate_unsupported" : response.code || "error",
        message: unsupported
          ? "当前宿主不支持从浏览器唤起 RDM，请在桌面端任务栏打开"
          : "无法在桌面端打开 RDM",
      };
    }
    if (response && (response.type === "activated" || response.type === "pong")) {
      setStatus("ok", true, "");
      return { ok: true, message: "已打开 RDM" };
    }
    return {
      ok: false,
      code: "activate_unsupported",
      message: "当前宿主不支持从浏览器唤起 RDM，请在桌面端任务栏打开",
    };
  } catch (err) {
    const text = String((err && err.message) || err);
    const classified = classifyHostError(text);
    setStatus(classified.host, false, classified.detail);
    return {
      ok: false,
      code: "host_unavailable",
      message: "无法连接本机 RDM，请在桌面端任务栏打开",
    };
  }
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
      refreshActivitySoon();
      return;
    }
    if (response && response.type === "error") {
      // duplicate（07 §4.3）：同一 URL 已在 RDM 下载 —— 不创建第二个任务、
      // 也绝不能回退浏览器下载（否则会得到两份文件）。
      if (response.code === "duplicate") {
        setStatus("ok", true, "");
        await pushRecent({
          fileName: request.file_name || fileName,
          url: redactUrl(url),
          status: "duplicate",
          detail: "该下载已在 RDM 中，未重复创建任务",
        });
        flashBadge("✓");
        refreshActivitySoon();
        return;
      }
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

const CONTEXT_MENU_IDS = [
  MENU_LINK_ID,
  MENU_IMAGE_ID,
  MENU_MEDIA_ID,
  MENU_PAGE_ID,
  MENU_SELECTION_ID,
  MENU_OPTIONS_ID,
  "rdm-menu-separator-1",
  "rdm-menu-separator-2",
];

/**
 * 右键菜单（07 §9）：短、按上下文显示、文案与桌面一致。
 * 解析入口始终显示（是否配置 AI 由桌面端决定，扩展无法读取）；
 * 子菜单（最近目录等）暂不做。
 */
async function ensureContextMenu(settings) {
  const current = settings || (await loadSettings());
  for (const id of CONTEXT_MENU_IDS) {
    try {
      await browser.contextMenus.remove(id);
    } catch (_err) {
      /* 不存在 */
    }
  }
  if (!current.contextMenu) return;
  const items = [
    { id: MENU_LINK_ID, title: "使用 RDM 下载链接", contexts: ["link"] },
    { id: MENU_IMAGE_ID, title: "用 RDM 下载图像", contexts: ["image"] },
    { id: MENU_MEDIA_ID, title: "用 RDM 下载媒体", contexts: ["audio", "video"] },
    {
      id: "rdm-menu-separator-1",
      type: "separator",
      contexts: ["link", "image", "audio", "video", "page", "selection"],
    },
    { id: MENU_PAGE_ID, title: "解析页面资源…", contexts: ["page"] },
    {
      id: MENU_SELECTION_ID,
      title: "用 RDM 下载选中的链接",
      contexts: ["selection"],
    },
    {
      id: "rdm-menu-separator-2",
      type: "separator",
      contexts: ["page", "selection"],
    },
    { id: MENU_OPTIONS_ID, title: "RDM 设置…", contexts: ["page", "selection"] },
  ];
  for (const item of items) {
    try {
      await browser.contextMenus.create(item);
    } catch (err) {
      debug("创建右键菜单失败（" + item.id + "）: " + err);
    }
  }
}

/** 右键转交（链接 / 图片 / 媒体 / 选中的链接）。 */
async function handleContextMenuClick(info, tab) {
  const settings = await loadSettings();
  let url = firstHttpUrl([info.linkUrl, info.srcUrl]);
  if (!url && typeof info.selectionText === "string" && info.selectionText.trim()) {
    const selected = parseUrlsFromText(info.selectionText, 1);
    url = selected.length > 0 ? selected[0] : "";
  }
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
      refreshActivitySoon();
      // 页面内提示默认关闭（07 §10）；只在用户主动右键且设置开启时注入。
      if (settings.pageToast && tab && Number.isInteger(tab.id)) {
        const name = request.file_name || fileNameFromUrl(url) || "文件";
        showPageToast(tab.id, "已交给 RDM · " + name).catch(() => {});
      }
    } else if (response && response.type === "error" && response.code === "duplicate") {
      setStatus("ok", true, "");
      await pushRecent({
        fileName: request.file_name || fileNameFromUrl(url),
        url: redactUrl(url),
        status: "duplicate",
        detail: "该下载已在 RDM 中，未重复创建任务",
      });
      flashBadge("✓");
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
    browser: detectBrowserKind(),
    activity: cachedActivity,
  };
}

browser.runtime.onMessage.addListener((message) => {
  if (!message || typeof message !== "object" || typeof message.type !== "string") {
    return Promise.resolve({ ok: false, error: "invalid_message" });
  }
  switch (message.type) {
    case "getState":
      return getState();
    case "getTabContext":
      return handleGetTabContext();
    case "getActivity":
      return getActivityResult();
    case "handoff":
      return handlePopupHandoff(message);
    case "openRdm":
      return handleOpenRdm();
    case "testConnection":
      return (async () => {
        const startedAt = Date.now();
        await refreshStatus();
        const snapshot = await getState();
        snapshot.latencyMs = Date.now() - startedAt;
        return snapshot;
      })();
    case "setSettings":
      return saveSettings(message.settings).then(() => getState());
    case "clearRecent":
      return clearRecent()
        .then(() => getState())
        .then((snapshot) => Object.assign(snapshot, { undone: false }));
    case "restoreRecent":
      return restoreRecent()
        .then(() => getState())
        .then((snapshot) => Object.assign(snapshot, { undone: true }));
    case "status":
      return fetchDownloadStatus().then((result) => {
        recordActivity(result);
        return result;
      });
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
  if (info.menuItemId === MENU_OPTIONS_ID) {
    browser.runtime.openOptionsPage().catch((err) => debug("打开设置失败: " + err));
    return;
  }
  if (info.menuItemId === MENU_PAGE_ID) {
    handleContextMenuAiParse(info, tab).catch((err) => {
      debug("右键解析页面资源失败: " + err);
    });
    return;
  }
  if (
    info.menuItemId !== MENU_LINK_ID &&
    info.menuItemId !== MENU_IMAGE_ID &&
    info.menuItemId !== MENU_MEDIA_ID &&
    info.menuItemId !== MENU_SELECTION_ID
  ) {
    return;
  }
  handleContextMenuClick(info, tab).catch((err) => {
    debug("右键转交失败: " + err);
  });
});

browser.alarms.onAlarm.addListener((alarm) => {
  if (!alarm || typeof alarm.name !== "string") return;
  if (alarm.name === PING_ALARM) {
    refreshStatus().catch(() => {});
    return;
  }
  if (alarm.name === ACTIVITY_ALARM) {
    // badge 活动数兜底轮询（07 §8）。alarms 最小 1 分钟，popup 打开期间
    // 有更实时的 1s 轮询，这里只保证关闭 popup 后 badge 不会长期失真。
    fetchDownloadStatus()
      .then((result) => recordActivity(result))
      .catch(() => {});
  }
});

browser.runtime.onInstalled.addListener((details) => {
  initialize()
    .then(() => ensureContextMenu())
    .then(() => {
      // 首次安装引导（07 §12）：打开设置页的 onboarding 卡。
      if (details && details.reason === "install") {
        return browser.runtime.openOptionsPage().catch(() => {});
      }
      return undefined;
    })
    .catch((err) => debug("初始化失败: " + err));
});

browser.runtime.onStartup.addListener(() => {
  initialize()
    .then(() => ensureContextMenu())
    .catch((err) => debug("初始化失败: " + err));
});

browser.storage.onChanged.addListener((changes, areaName) => {
  if (areaName !== "local") return;
  if (changes[SETTINGS_KEY]) {
    cachedSettings = normalizeSettings(changes[SETTINGS_KEY].newValue);
    updateBadge();
  }
  if (changes[ACTIVITY_KEY]) {
    cachedActivity = normalizeStoredActivity(changes[ACTIVITY_KEY].newValue);
    updateBadge();
  }
});

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

const STATUS_MAX_AGE_MS = 5 * 60 * 1000;

async function initialize() {
  await loadSettings();
  status = await loadStoredStatus();
  await loadStoredActivity();
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


