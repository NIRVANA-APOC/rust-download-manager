/*!
 * RDM 下载接管 — popup 逻辑。
 * 只负责渲染后台 getState/testConnection/status 返回的快照；所有决策在 background.js。
 *
 * 进度展示：状态卡之后新增「正在下载」区（最多 3 条，含线程数、溢出与不可用提示），
 * 每 1s 轮询 status 并做增量 DOM 更新；「最近转交」只保留状态徽章，不再重复进度条。
 */
"use strict";

/* global RDMPure */
const {
  formatAge,
  normalizeDownloads,
  progressPercent,
  formatSpeed,
  formatActiveText,
  pickActiveDownloads,
} = RDMPure;

const STATUS_LABELS = {
  handed: "已转交",
  queued: "已排队",
  fallback: "已回退",
  ai: "AI 解析",
  error: "失败",
};

/** popup 打开期间每 1000ms 查询一次下载进度（冻结契约）。 */
const STATUS_POLL_MS = 1000;

/** status 失败 code → 可操作提示（单行/两行小字）。 */
const STATUS_HINTS = {
  host_outdated:
    "进度不可用：原生宿主版本过旧，请重新运行 scripts/install-firefox-host.ps1 并重启浏览器",
  timeout: "进度不可用：原生宿主无响应，请重启浏览器后重试",
  host_unavailable: "进度不可用：原生宿主无响应，请重启浏览器后重试",
  error: "进度不可用：读取下载状态失败，请重启浏览器后重试",
};

/** GUI 离线是"ok 响应但不是错误"的降级路径。 */
const GUI_OFFLINE_HINT = "RDM GUI 未运行：下载会先排队，启动后这里显示进度";

const els = {
  enabled: document.getElementById("enabled"),
  version: document.getElementById("version"),
  hostDot: document.getElementById("host-dot"),
  hostText: document.getElementById("host-text"),
  guiDot: document.getElementById("gui-dot"),
  guiText: document.getElementById("gui-text"),
  detail: document.getElementById("detail"),
  refresh: document.getElementById("refresh"),
  options: document.getElementById("options"),
  clear: document.getElementById("clear"),
  recentList: document.getElementById("recent-list"),
  hostName: document.getElementById("host-name"),
  aiParse: document.getElementById("ai-parse"),
  aiCredentials: document.getElementById("ai-credentials"),
  aiResult: document.getElementById("ai-result"),
  activeSection: document.getElementById("active-section"),
  activeHead: document.getElementById("active-head"),
  activeTitle: document.getElementById("active-title"),
  activeSummary: document.getElementById("active-summary"),
  activeList: document.getElementById("active-list"),
  activeMore: document.getElementById("active-more"),
  activeHint: document.getElementById("active-hint"),
};

let currentState = null;
let ageTimer = null;
let statusTimer = null;
let statusInFlight = false;
let closed = false;
let lastStatus = null;
/** 「正在下载」区复用的行节点（增量更新，不重建 section）。 */
let activeRows = [];

function send(message) {
  return browser.runtime.sendMessage(message).catch((err) => ({
    ok: false,
    error: String(err),
  }));
}

function setDot(element, kind) {
  element.className = "dot " + kind;
}

function renderStatus(state) {
  const status = (state && state.status) || { host: "unknown", gui: false, detail: "" };
  const settings = state && state.settings ? state.settings : { enabled: true };

  if (settings.enabled === false) {
    els.hostText.textContent = "下载接管已关闭";
    setDot(els.hostDot, "unknown");
    els.guiText.textContent = "开启后生效";
    setDot(els.guiDot, "unknown");
    els.detail.textContent = "";
    return;
  }

  let hostText = "原生宿主：检测中…";
  let hostKind = "unknown";
  if (status.host === "ok") {
    hostText = "原生宿主：已连接";
    if (status.hostVersion) hostText += "（v" + status.hostVersion + "）";
    hostKind = "ok";
  } else if (status.host === "missing") {
    hostText = "原生宿主：未安装";
    hostKind = "bad";
  } else if (status.host === "error") {
    hostText = "原生宿主：连接异常";
    hostKind = "bad";
  }
  els.hostText.textContent = hostText;
  setDot(els.hostDot, hostKind);

  let guiText = "RDM GUI：未知";
  let guiKind = "unknown";
  if (status.host === "ok") {
    if (status.gui) {
      guiText = "RDM GUI：运行中";
      guiKind = "ok";
    } else {
      guiText = "RDM GUI：未运行（下载会先排队）";
      guiKind = "warn";
    }
  } else if (status.host === "missing") {
    guiText = "RDM GUI：不可用";
  }
  els.guiText.textContent = guiText;
  setDot(els.guiDot, guiKind);

  els.detail.textContent = status.detail || "";
}

function createElement(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined && text !== null) element.textContent = text;
  return element;
}

// ---------------------------------------------------------------------------
// 「最近转交」（只保留状态徽章，不再重复进度条）
// ---------------------------------------------------------------------------

function renderRecent(state) {
  const list = (state && state.recent) || [];
  els.recentList.textContent = "";
  if (!Array.isArray(list) || list.length === 0) {
    els.recentList.appendChild(createElement("li", "empty", "暂无记录"));
    return;
  }
  for (const entry of list) {
    if (!entry || typeof entry !== "object") continue;
    const item = createElement("li", "item");

    const top = createElement("div", "item-top");
    const name = createElement("span", "item-name", entry.fileName || "（未知文件）");
    name.title = entry.url || "";
    const statusKey = STATUS_LABELS[entry.status] ? entry.status : "error";
    const badge = createElement("span", "badge " + statusKey, STATUS_LABELS[statusKey]);
    top.appendChild(name);
    top.appendChild(badge);

    const meta = createElement("div", "item-meta");
    const url = createElement("span", "item-url", entry.url || "");
    url.title = entry.url || "";
    const age = createElement("span", "item-age", formatAge(entry.at));
    age.dataset.at = String(entry.at || 0);
    meta.appendChild(url);
    meta.appendChild(age);

    item.appendChild(top);
    item.appendChild(meta);
    if (entry.detail) {
      item.appendChild(createElement("div", "item-detail", entry.detail));
    }
    els.recentList.appendChild(item);
  }
}

// ---------------------------------------------------------------------------
// 「正在下载」进度区（状态卡之后、AI 卡之前）
// ---------------------------------------------------------------------------

function createActiveRow() {
  const li = createElement("li", "active-item");
  const top = createElement("div", "active-top");
  const name = createElement("span", "active-name");
  const badge = createElement("span", "active-badge");
  top.appendChild(name);
  top.appendChild(badge);

  const bar = createElement("div", "progress");
  bar.setAttribute("role", "progressbar");
  bar.setAttribute("aria-valuemin", "0");
  bar.setAttribute("aria-valuemax", "100");
  bar.setAttribute("aria-label", "下载进度");
  const fill = createElement("div", "progress-fill");
  bar.appendChild(fill);

  const text = createElement("span", "active-text");
  li.appendChild(top);
  li.appendChild(bar);
  li.appendChild(text);
  els.activeList.appendChild(li);
  return { li, name, badge, bar, fill, text, lastState: "", lastText: null };
}

function ensureActiveRow(index) {
  while (activeRows.length <= index) activeRows.push(createActiveRow());
  return activeRows[index];
}

function hideActiveRow(row) {
  if (!row.li.hidden) row.li.hidden = true;
  row.lastText = null;
}

/** 只更新单条 active 行的节点（不重建列表）。 */
function renderActiveRow(row, download) {
  const state = download.state === "paused" ? "paused" : "downloading";
  if (row.lastState !== state) {
    row.li.className = "active-item state-" + state;
    row.badge.className = "active-badge " + state;
    row.badge.textContent = state === "paused" ? "已暂停" : "下载中";
    row.bar.className = "progress state-" + state;
    row.lastState = state;
  }
  row.name.textContent = download.fileName || "（未知文件）";
  row.name.title = download.fileName || "";

  const percent = progressPercent(download);
  // 总大小未知（total=null）→ 不确定态条纹，不显示百分比。
  const indeterminate = percent === null;
  if (row.bar.classList.contains("indeterminate") !== indeterminate) {
    row.bar.classList.toggle("indeterminate", indeterminate);
  }
  const width = indeterminate ? "" : percent + "%";
  if (row.fill.style.width !== width) row.fill.style.width = width;
  if (indeterminate) {
    if (row.bar.hasAttribute("aria-valuenow")) row.bar.removeAttribute("aria-valuenow");
  } else {
    row.bar.setAttribute("aria-valuenow", String(percent));
  }

  const text = formatActiveText(download);
  if (row.lastText !== text) {
    row.text.textContent = text;
    row.bar.setAttribute("aria-valuetext", text);
    row.lastText = text;
  }
  if (row.li.hidden) row.li.hidden = false;
}

/** 活跃下载（含被截断的隐藏条目）的速度总和，用于 header「总速度」。 */
function totalActiveSpeed(downloads) {
  let speed = 0;
  for (const download of downloads) {
    if (download.state === "downloading" || download.state === "paused") {
      speed += download.speedBps || 0;
    }
  }
  return speed;
}

/** status 结果 → 可操作提示（失败 / GUI 未运行）；正常且有进度时返回 "". */
function statusHintFor(result) {
  if (!result || typeof result !== "object") return "";
  if (result.ok !== true) {
    const code = typeof result.code === "string" ? result.code : "error";
    return STATUS_HINTS[code] || STATUS_HINTS.error;
  }
  if (result.guiRunning !== true) return GUI_OFFLINE_HINT;
  return "";
}

/** status 快照 → 「正在下载」区（无 active 时整段隐藏；失败时显示提示）。 */
function renderActiveStatus(result) {
  lastStatus = result && typeof result === "object" ? result : null;
  const ok = !!lastStatus && lastStatus.ok === true;
  const downloads = ok ? normalizeDownloads(lastStatus.downloads) : [];
  const picked = pickActiveDownloads(downloads);
  const hint = statusHintFor(lastStatus);

  const showSection = picked.total > 0 || hint !== "";
  els.activeSection.hidden = !showSection;
  els.activeHead.hidden = picked.total === 0;
  els.activeList.hidden = picked.total === 0;
  els.activeMore.hidden = picked.overflow <= 0;
  els.activeMore.textContent = picked.overflow > 0 ? "还有 " + picked.overflow + " 个…" : "";

  if (hint) {
    els.activeHint.textContent = hint;
    els.activeHint.hidden = false;
    els.activeHint.dataset.code =
      typeof lastStatus.code === "string" ? lastStatus.code : ok ? "gui" : "error";
  } else {
    els.activeHint.textContent = "";
    els.activeHint.hidden = true;
    if (els.activeHint.dataset) delete els.activeHint.dataset.code;
  }

  if (picked.total > 0) {
    els.activeTitle.textContent = "正在下载 " + picked.total;
    const speed =
      lastStatus.globalSpeedBps > 0 ? lastStatus.globalSpeedBps : totalActiveSpeed(downloads);
    const speedText = formatSpeed(speed);
    els.activeSummary.textContent = speedText ? "总速度 " + speedText : "";
  } else {
    els.activeTitle.textContent = "正在下载";
    els.activeSummary.textContent = "";
  }

  // 行节点复用：先补齐，再逐行更新/隐藏（不重建列表）。
  while (activeRows.length < picked.visible.length) {
    ensureActiveRow(activeRows.length);
  }
  for (let index = 0; index < activeRows.length; index += 1) {
    const row = activeRows[index];
    if (index < picked.visible.length) {
      renderActiveRow(row, picked.visible[index]);
    } else {
      hideActiveRow(row);
    }
  }
}

async function pollStatus() {
  if (closed || statusInFlight) return; // 已在进行的请求不重复发
  statusInFlight = true;
  try {
    const result = await send({ type: "status" });
    if (closed) return;
    renderActiveStatus(result);
  } finally {
    statusInFlight = false;
  }
}

function startStatusPolling() {
  if (closed || statusTimer) return;
  statusTimer = setInterval(() => {
    pollStatus().catch(() => {});
  }, STATUS_POLL_MS);
  pollStatus().catch(() => {});
}

function cleanup() {
  closed = true;
  if (ageTimer) {
    clearInterval(ageTimer);
    ageTimer = null;
  }
  if (statusTimer) {
    clearInterval(statusTimer);
    statusTimer = null;
  }
  activeRows = [];
  lastStatus = null;
}

// ---------------------------------------------------------------------------
// 状态卡 / 设置 / AI 解析
// ---------------------------------------------------------------------------

function render(state) {
  if (!state || !state.ok) {
    renderStatus({ status: { host: "error", detail: (state && state.error) || "后台无响应" } });
    return;
  }
  currentState = state;
  if (state.settings) {
    els.enabled.checked = state.settings.enabled !== false;
    els.aiCredentials.checked = state.settings.aiIncludeCredentials === true;
  }
  if (state.extensionVersion) {
    els.version.textContent = "扩展 v" + state.extensionVersion;
  }
  if (state.nativeHost) {
    els.hostName.textContent = state.nativeHost;
  }
  renderStatus(state);
  renderRecent(state);
}

function refreshAges() {
  for (const element of els.recentList.querySelectorAll("[data-at]")) {
    const at = Number(element.dataset.at);
    element.textContent = formatAge(at);
  }
}

async function loadState() {
  const state = await send({ type: "getState" });
  if (state && state.ok) render(state);
}

async function detect() {
  els.refresh.disabled = true;
  els.refresh.textContent = "检测中…";
  try {
    const state = await send({ type: "testConnection" });
    render(state);
  } finally {
    els.refresh.disabled = false;
    els.refresh.textContent = "重新检测";
  }
}

function setAiResult(text, kind) {
  els.aiResult.textContent = text || "";
  els.aiResult.className = "ai-result" + (kind ? " " + kind : "");
}

async function runAiParse() {
  const includeCredentials = els.aiCredentials.checked === true;
  els.aiParse.disabled = true;
  els.aiParse.textContent = "解析中…";
  setAiResult("正在发送到 RDM…", "busy");
  try {
    const result = await send({ type: "aiParse", includeCredentials });
    if (result && result.ok) {
      setAiResult(result.message || "已发送到 RDM，解析结果在桌面端 AI 栏查看", "ok");
    } else {
      setAiResult((result && result.message) || "AI 解析未发送", "bad");
    }
  } finally {
    els.aiParse.disabled = false;
    els.aiParse.textContent = "解析此页";
  }
}

function bindEvents() {
  els.enabled.addEventListener("change", async () => {
    const enabled = els.enabled.checked;
    const state = await send({ type: "setSettings", settings: { enabled } });
    render(state);
  });

  els.refresh.addEventListener("click", () => {
    detect().catch(() => {});
  });

  els.options.addEventListener("click", () => {
    browser.runtime.openOptionsPage().catch(() => {});
  });

  els.clear.addEventListener("click", async () => {
    const state = await send({ type: "clearRecent" });
    render(state);
  });

  els.aiCredentials.addEventListener("change", async () => {
    const state = await send({
      type: "setSettings",
      settings: { aiIncludeCredentials: els.aiCredentials.checked === true },
    });
    render(state);
  });

  els.aiParse.addEventListener("click", () => {
    runAiParse().catch((err) => setAiResult("AI 解析失败：" + err, "bad"));
  });
}

async function init() {
  els.activeSection.hidden = true;
  els.activeHead.hidden = true;
  els.activeList.hidden = true;
  els.activeMore.hidden = true;
  els.activeHint.hidden = true;
  bindEvents();
  await loadState();
  await detect();
  ageTimer = setInterval(refreshAges, 1000);
  startStatusPolling();
}

window.addEventListener("unload", cleanup);
window.addEventListener("pagehide", cleanup);

init().catch((err) => {
  renderStatus({ status: { host: "error", detail: "popup 初始化失败：" + err } });
});
