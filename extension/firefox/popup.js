/*!
 * RDM 下载接管 — popup 逻辑（M4 重做）。
 *
 * 信息架构（docs/design/07-browser-extension.md §3–§7）：
 *   头部：品牌 + 弱化「已就绪」+ 接管开关 + 设置
 *   状态条：只在异常（宿主未安装 / GUI 未运行 / 协议错误 / 接管关闭）时出现
 *   主操作：唯一蓝色主按钮，文案随当前 tab 上下文变化
 *   正在下载：最多 3 条活动任务 + 进度/速度，头部「打开 RDM」
 *   最近活动：最多 5 条，清空后 toast 可撤销
 *   设置入口：仅头部齿轮（打开 options 页；版本与帮助只在 options 页展示）
 *
 * 所有决策（主按钮文案、状态条、badge 语义、最近活动模型）都在
 * lib/pure.js 的纯函数里，本文件只负责 DOM 与消息收发。
 */
"use strict";

/* global RDMPure */
const {
  normalizeSettings,
  normalizeDownloads,
  pickActiveDownloads,
  popupActiveRow,
  formatActiveHeadline,
  readyStateLabel,
  describeBanner,
  choosePrimaryAction,
  formatTabSubtitle,
  buildRecentItems,
  formatAge,
  formatHandoffResult,
  describeActivityFailure,
  connectionCommands,
} = RDMPure;

/** popup 打开期间活动下载轮询间隔（07 §6：现有 1s 保留）。 */
const STATUS_POLL_MS = 1000;
/** 连接检测上限（07 §5：3s 超时）。 */
const DETECT_TIMEOUT_MS = 3200;
/** 转交中的兜底超时（07 §4.3：16s）。 */
const HANDOFF_TIMEOUT_MS = 16000;
/** 主按钮成功后短暂反馈时长（1.5–2.5s）。 */
const RESULT_TTL_MS = 2500;
/** 清空撤销窗口（07 §7：5s）。 */
const UNDO_TTL_MS = 5000;

const els = {
  enabled: document.getElementById("enabled"),
  settings: document.getElementById("settings"),
  ready: document.getElementById("ready"),
  readyDot: document.getElementById("ready-dot"),
  readyText: document.getElementById("ready-text"),
  chromeBadge: document.getElementById("chrome-badge"),
  banner: document.getElementById("banner"),
  bannerIcon: document.getElementById("banner-icon"),
  bannerText: document.getElementById("banner-text"),
  bannerAction: document.getElementById("banner-action"),
  primary: document.getElementById("primary"),
  primaryText: document.getElementById("primary-text"),
  primaryInfo: document.getElementById("primary-info"),
  primaryResult: document.getElementById("primary-result"),
  activeSection: document.getElementById("active-section"),
  activeHead: document.getElementById("active-head"),
  activeTitle: document.getElementById("active-title"),
  activeSpeed: document.getElementById("active-speed"),
  activeOpen: document.getElementById("active-open"),
  activeList: document.getElementById("active-list"),
  activeMore: document.getElementById("active-more"),
  activeHint: document.getElementById("active-hint"),
  recentList: document.getElementById("recent-list"),
  recentEmpty: document.getElementById("recent-empty"),
  recentClear: document.getElementById("recent-clear"),
  undoToast: document.getElementById("undo-toast"),
  undoText: document.getElementById("undo-text"),
  undoAction: document.getElementById("undo-action"),
};

/** getState / setSettings 返回的最近一次快照。 */
let state = null;
/** 当前 tab 上下文（background.getTabContext 返回）。 */
let tabContext = null;
/** 当前主按钮决策（choosePrimaryAction 的输出）。 */
let currentAction = null;
/** 最近一次活动快照（getActivity 返回）。 */
let lastActivity = null;
/** 主按钮提交中（防重复点击）。 */
let busy = false;
let closed = false;

let pollTimer = null;
let ageTimer = null;
let pollInFlight = false;
let resultTimer = null;
let undo = null;

/** 「正在下载」行节点（最多 3 条，增量复用避免闪烁）。 */
const activeRows = [];
/** 最近活动的时间节点，用于每秒刷新相对时间。 */
let recentAgeRefs = [];
/** 最近活动渲染签名：内容不变时不重建 DOM（避免 1s 轮询导致闪变）。 */
let recentSignature = "";

// ---------------------------------------------------------------------------
// 消息与工具
// ---------------------------------------------------------------------------

function send(message) {
  try {
    return browser.runtime.sendMessage(message).catch((err) => ({
      ok: false,
      error: String(err),
    }));
  } catch (err) {
    return Promise.resolve({ ok: false, error: String(err) });
  }
}

/** 带超时的消息（连接/转交失败时也要落地成可读状态，不无限等待）。 */
function sendWithTimeout(message, timeoutMs) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };
    const timer = setTimeout(() => {
      finish({ ok: false, code: "timeout", error: "请求超时" });
    }, timeoutMs);
    send(message).then((value) => {
      if (settled) return;
      clearTimeout(timer);
      finish(value);
    });
  });
}

function settingsSnapshot() {
  return state && state.settings ? state.settings : {};
}

function statusSnapshot() {
  if (state && state.status) return state.status;
  if (lastActivity && lastActivity.ok === true) {
    return { host: "ok", gui: lastActivity.guiRunning === true, detail: "" };
  }
  return { host: "unknown", gui: false, detail: "" };
}

function createElement(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined && text !== null) element.textContent = text;
  return element;
}

function setHidden(element, hidden) {
  if (element) element.hidden = hidden === true;
}

/** 统一应用一份 getState 快照（getState / testConnection / setSettings 等）。 */
function applyState(next) {
  if (!next || next.ok !== true) return false;
  state = {
    ok: true,
    settings: next.settings,
    status: next.status,
    recent: Array.isArray(next.recent) ? next.recent : [],
    extensionVersion: next.extensionVersion,
    nativeHost: next.nativeHost,
    browser: next.browser,
    activity: next.activity || null,
  };
  if (els.chromeBadge) els.chromeBadge.hidden = next.browser !== "chrome";
  if (state.activity) {
    lastActivity = state.activity;
  }
  renderAll();
  return true;
}

// ---------------------------------------------------------------------------
// 渲染：头部 / 状态条 / 主按钮
// ---------------------------------------------------------------------------

function renderConnection() {
  const settings = normalizeSettings(settingsSnapshot());
  const status = statusSnapshot();
  if (els.enabled) els.enabled.checked = settings.enabled !== false;

  const ready = readyStateLabel({ settings, status });
  if (els.readyText) els.readyText.textContent = ready.text;
  if (els.readyDot) els.readyDot.className = "dot " + ready.tone;

  const banner = describeBanner({ settings, status });
  if (!banner) {
    setHidden(els.banner, true);
    if (els.bannerAction) els.bannerAction.hidden = true;
    return;
  }
  if (els.banner) {
    els.banner.hidden = false;
    els.banner.className = "banner " + banner.kind;
  }
  if (els.bannerIcon) {
    els.bannerIcon.textContent = banner.kind === "info" ? "i" : "!";
  }
  if (els.bannerText) els.bannerText.textContent = banner.text;
  if (els.bannerAction) {
    els.bannerAction.hidden = false;
    els.bannerAction.textContent = banner.action.label;
    els.bannerAction.dataset.action = banner.action.id;
  }
}

function busyLabel(action) {
  if (!action) return "正在处理…";
  return action.kind === "parse-page" ? "解析中…" : "正在转交…";
}

function renderPrimary() {
  const action = choosePrimaryAction({
    settings: settingsSnapshot(),
    status: statusSnapshot(),
    tabContext: tabContext || {},
  });
  currentAction = action;
  if (els.primaryText) {
    els.primaryText.textContent = busy ? busyLabel(action) : action.label;
  }
  if (els.primary) {
    els.primary.disabled = busy || action.disabled === true;
    if (els.primary.classList && els.primary.classList.toggle) {
      els.primary.classList.toggle("busy", busy);
    }
  }
  if (els.primaryInfo) {
    els.primaryInfo.textContent = tabContext ? formatTabSubtitle(tabContext) : "";
  }
}

function showResult(text, tone, ttlMs) {
  if (!els.primaryResult) return;
  els.primaryResult.textContent = text || "";
  els.primaryResult.className = "primary-result" + (tone ? " " + tone : "");
  if (resultTimer) clearTimeout(resultTimer);
  resultTimer = null;
  if (!text) return;
  resultTimer = setTimeout(() => {
    resultTimer = null;
    if (els.primaryResult) {
      els.primaryResult.textContent = "";
      els.primaryResult.className = "primary-result";
    }
  }, typeof ttlMs === "number" ? ttlMs : RESULT_TTL_MS);
}

// ---------------------------------------------------------------------------
// 渲染：「正在下载」
// ---------------------------------------------------------------------------

function createActiveRow() {
  const item = createElement("li", "active-item");
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
  item.appendChild(top);
  item.appendChild(bar);
  item.appendChild(text);
  els.activeList.appendChild(item);
  return { item, name, badge, bar, fill, text, lastState: "", lastText: "", lastPercent: null };
}

function ensureActiveRow(index) {
  while (activeRows.length <= index) activeRows.push(createActiveRow());
  return activeRows[index];
}

function hideActiveRow(row) {
  if (!row) return;
  if (!row.item.hidden) row.item.hidden = true;
  row.lastText = "";
}

function renderActiveRow(row, download) {
  const model = popupActiveRow(download);
  if (!model) {
    hideActiveRow(row);
    return;
  }
  if (row.lastState !== model.state) {
    row.item.className = "active-item state-" + model.state;
    row.badge.className = "active-badge " + model.state;
    row.badge.textContent = model.stateLabel;
    row.bar.className = "progress state-" + model.state;
    row.lastState = model.state;
  }
  const name = download.fileName || "（未知文件）";
  if (row.name.textContent !== name) row.name.textContent = name;
  row.name.title = name;

  if (row.bar.classList.contains("indeterminate") !== model.indeterminate) {
    row.bar.classList.toggle("indeterminate", model.indeterminate);
  }
  if (model.indeterminate) {
    if (row.bar.hasAttribute("aria-valuenow")) row.bar.removeAttribute("aria-valuenow");
    if (row.fill.style.width !== "") row.fill.style.width = "";
  } else {
    row.bar.setAttribute("aria-valuenow", String(model.percent));
    const width = model.percent + "%";
    if (row.fill.style.width !== width) row.fill.style.width = width;
  }
  if (row.lastText !== model.detail) {
    row.text.textContent = model.detail;
    row.bar.setAttribute("aria-valuetext", model.detail);
    row.lastText = model.detail;
  }
  if (row.item.hidden) row.item.hidden = false;
}

function renderActive(activity) {
  if (!els.activeSection) return;
  const result = activity && typeof activity === "object" ? activity : null;
  const ok = !!result && result.ok === true;
  const downloads = ok ? normalizeDownloads(result.downloads) : [];
  const picked = pickActiveDownloads(downloads);
  const headline = formatActiveHeadline(downloads, ok ? result.globalSpeedBps : 0);
  // 宿主缺失/连接异常时状态条已经说明问题，活动区不再重复报错（07 §5）。
  const hostReady = statusSnapshot().host === "ok";
  const failureText = ok || !hostReady ? "" : describeActivityFailure(result);
  const showSection = picked.total > 0 || failureText !== "";
  setHidden(els.activeSection, !showSection);
  if (!showSection) {
    setHidden(els.activeHead, true);
    setHidden(els.activeMore, true);
    setHidden(els.activeHint, true);
    return;
  }
  setHidden(els.activeHead, picked.total === 0);
  if (els.activeTitle) els.activeTitle.textContent = headline.title;
  if (els.activeSpeed) els.activeSpeed.textContent = headline.speedText;
  if (els.activeMore) {
    els.activeMore.textContent = headline.overflowText;
    els.activeMore.hidden = picked.overflow <= 0;
  }
  if (els.activeHint) {
    if (failureText) {
      els.activeHint.textContent = failureText;
      els.activeHint.hidden = false;
      els.activeHint.dataset.code = (result && result.code) || "error";
    } else {
      els.activeHint.textContent = "";
      els.activeHint.hidden = true;
      if (els.activeHint.dataset) delete els.activeHint.dataset.code;
    }
  }
  setHidden(els.activeList, picked.total === 0);
  const visible = picked.visible;
  while (activeRows.length < visible.length) ensureActiveRow(activeRows.length);
  for (let index = 0; index < activeRows.length; index += 1) {
    if (index < visible.length) renderActiveRow(activeRows[index], visible[index]);
    else hideActiveRow(activeRows[index]);
  }
}

// ---------------------------------------------------------------------------
// 渲染：最近活动（最多 5 条）
// ---------------------------------------------------------------------------

function renderRecent(force) {
  if (!els.recentList) return;
  const list = state && Array.isArray(state.recent) ? state.recent : [];
  const items = buildRecentItems(list, { limit: 5 });
  if (els.recentClear) els.recentClear.disabled = items.length === 0;
  setHidden(els.recentEmpty, items.length > 0);
  const signature = items.map((item) => item.key + "|" + item.status).join("~");
  if (!force && signature === recentSignature) return;
  recentSignature = signature;
  els.recentList.textContent = "";
  recentAgeRefs = [];
  for (const item of items) {
    const row = createElement("li", "recent-item type-" + item.type);
    row.setAttribute("role", "button");
    row.setAttribute("tabindex", "0");
    row.title = item.detail || item.url || item.name;
    row.dataset.at = String(item.at || 0);
    row.appendChild(
      createElement("span", "recent-icon", item.type === "parse" ? "⚡" : "⬇")
    );
    row.appendChild(createElement("span", "recent-name", item.name));
    row.appendChild(createElement("span", "recent-status " + item.status, item.statusLabel));
    const age = createElement("span", "recent-age", item.ageText);
    age.dataset.at = String(item.at || 0);
    row.appendChild(age);
    recentAgeRefs.push({ element: age, at: item.at });
    const activate = () => {
      openRdm().catch(() => {});
    };
    row.addEventListener("click", activate);
    row.addEventListener("keydown", (event) => {
      if (!event || event.key === "Enter" || event.key === " ") activate();
    });
    els.recentList.appendChild(row);
  }
}

function refreshAges() {
  if (!recentAgeRefs.length) return;
  for (const ref of recentAgeRefs) {
    const text = formatAge(ref.at);
    if (ref.element.textContent !== text) ref.element.textContent = text;
  }
}

// ---------------------------------------------------------------------------
// 状态快照应用 + 轮询
// ---------------------------------------------------------------------------

function renderAll() {
  renderConnection();
  renderPrimary();
  renderRecent(true);
  renderActive(lastActivity);
}

function applyActivity(result) {
  lastActivity = result && typeof result === "object" ? result : { ok: false, code: "error" };
  if (lastActivity.ok === true && state && state.status && state.status.host === "ok") {
    state.status = Object.assign({}, state.status, { gui: lastActivity.guiRunning === true });
    renderConnection();
    renderPrimary();
  }
  if (Array.isArray(lastActivity.recent)) {
    state = state || { ok: true, settings: {}, status: { host: "unknown" }, recent: [] };
    state.recent = lastActivity.recent;
    renderRecent();
  }
  renderActive(lastActivity);
}

async function pollActivity() {
  if (closed || pollInFlight) return;
  pollInFlight = true;
  try {
    const result = await send({ type: "getActivity" });
    if (closed) return;
    applyActivity(result);
  } finally {
    pollInFlight = false;
  }
}

function startPolling() {
  if (closed || pollTimer) return;
  pollTimer = setInterval(() => {
    pollActivity().catch(() => {});
  }, STATUS_POLL_MS);
  if (!ageTimer) {
    ageTimer = setInterval(refreshAges, STATUS_POLL_MS);
  }
  pollActivity().catch(() => {});
}

function cleanup() {
  closed = true;
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  if (ageTimer) {
    clearInterval(ageTimer);
    ageTimer = null;
  }
  if (resultTimer) {
    clearTimeout(resultTimer);
    resultTimer = null;
  }
  if (undo && undo.timer) {
    clearTimeout(undo.timer);
    undo = null;
  }
}

// ---------------------------------------------------------------------------
// 动作
// ---------------------------------------------------------------------------

/** 旧式复制兜底：没有 navigator.clipboard 或写入被拒时使用。 */
function legacyCopy(text) {
  try {
    if (!document.body || typeof document.execCommand !== "function") return false;
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const copied = document.execCommand("copy") === true;
    document.body.removeChild(area);
    return copied;
  } catch (_err) {
    return false;
  }
}

function copyText(text) {
  if (typeof text !== "string" || !text) return Promise.resolve(false);
  try {
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      return navigator.clipboard.writeText(text).then(
        () => true,
        () => legacyCopy(text)
      );
    }
  } catch (_err) {
    /* 降级 */
  }
  return Promise.resolve(legacyCopy(text));
}

async function copyInstallCommand() {
  const commands = connectionCommands(state && state.browser);
  const copied = await copyText(commands.installCommand);
  if (copied) {
    showResult("已复制安装命令", "ok");
  } else {
    showResult("复制失败，请在设置 → 连接诊断中手动复制", "danger");
  }
}

async function openRdm() {
  const result = await send({ type: "openRdm" });
  if (result && result.ok) {
    showResult("已在桌面端打开 RDM", "ok");
  } else {
    showResult(
      (result && result.message) || "请在桌面端任务栏打开 RDM",
      "warn"
    );
  }
}

function openOptions() {
  try {
    browser.runtime.openOptionsPage().catch(() => {});
  } catch (_err) {
    /* 无 options 页 */
  }
}

async function setEnabled(next) {
  const previous = normalizeSettings(settingsSnapshot()).enabled;
  state = state || { ok: true, settings: {}, status: { host: "unknown" }, recent: [] };
  state.settings = Object.assign({}, settingsSnapshot(), { enabled: next === true });
  renderAll();
  const result = await send({ type: "setSettings", settings: { enabled: next === true } });
  if (result && result.ok) {
    applyState(result);
  } else {
    state.settings = Object.assign({}, settingsSnapshot(), { enabled: previous });
    renderAll();
    showResult("开关未保存，请重试", "danger");
  }
}

async function detect() {
  const result = await sendWithTimeout({ type: "testConnection" }, DETECT_TIMEOUT_MS);
  if (closed) return;
  if (result && result.ok === true) {
    applyState(result);
    return;
  }
  const code = (result && (result.code || result.error)) || "error";
  state = state || { ok: true, settings: {}, status: { host: "unknown" }, recent: [] };
  state.status = {
    host: /missing|not found|未安装/i.test(String(code)) ? "missing" : "error",
    gui: false,
    detail: typeof code === "string" ? code : "error",
    checkedAt: Date.now(),
  };
  renderConnection();
  renderPrimary();
}

async function runPrimary(options) {
  const action = currentAction;
  if (!action || busy) return;
  if (action.kind === "install-host") {
    await copyInstallCommand();
    return;
  }
  if (action.kind === "retry-detect" || action.kind === "detect") {
    await detect();
    return;
  }
  if (action.kind === "enable") {
    await setEnabled(true);
    return;
  }
  if (action.kind === "unsupported" || action.disabled) return;

  const forceLaunch = (options && options.forceLaunch === true) || action.forceLaunch === true;
  busy = true;
  renderPrimary();
  showResult("");
  try {
    if (action.kind === "parse-page") {
      const result = await sendWithTimeout(
        { type: "aiParse", forceLaunch },
        HANDOFF_TIMEOUT_MS
      );
      if (result && result.ok === true) {
        showResult(result.message || "已发送到 RDM，解析结果在桌面端打开", "ok");
        await pollActivity();
      } else {
        showResult(
          (result && result.message) || "解析未发送：RDM 没有响应",
          "danger"
        );
      }
      return;
    }

    const urls =
      action.kind === "download-selection"
        ? (tabContext && tabContext.selectedUrls) || []
        : tabContext && tabContext.url
          ? [tabContext.url]
          : [];
    const result = await sendWithTimeout(
      {
        type: "handoff",
        urls,
        fileName:
          action.kind === "download-file" && tabContext ? tabContext.fileName || "" : "",
        forceLaunch,
      },
      HANDOFF_TIMEOUT_MS
    );
    const feedback = formatHandoffResult(result);
    showResult(feedback.text, feedback.tone);
    if (result && result.ok === true) {
      await pollActivity();
    }
  } finally {
    busy = false;
    renderPrimary();
  }
}

// ---------------------------------------------------------------------------
// 清空最近活动 + 撤销
// ---------------------------------------------------------------------------

function hideUndo() {
  if (undo && undo.timer) clearTimeout(undo.timer);
  undo = null;
  setHidden(els.undoToast, true);
}

function showUndo() {
  setHidden(els.undoToast, false);
  if (els.undoText) els.undoText.textContent = "已清空最近活动";
  const timer = setTimeout(() => {
    if (undo && undo.timer === timer) hideUndo();
  }, UNDO_TTL_MS);
  undo = { timer };
}

async function clearRecent() {
  const result = await send({ type: "clearRecent" });
  if (!(result && result.ok === true)) {
    showResult("清空失败，请重试", "danger");
    return;
  }
  applyState(result);
  showUndo();
}

async function undoClear() {
  if (!undo) return;
  const result = await send({ type: "restoreRecent" });
  hideUndo();
  if (result && result.ok === true) {
    applyState(result);
  } else {
    showResult("撤销失败，请重试", "danger");
  }
}

// ---------------------------------------------------------------------------
// 事件绑定
// ---------------------------------------------------------------------------

function bindEvents() {
  if (els.enabled) {
    els.enabled.addEventListener("change", () => {
      setEnabled(els.enabled.checked === true).catch(() => {});
    });
  }
  if (els.primary) {
    els.primary.addEventListener("click", () => {
      runPrimary().catch((err) => showResult("操作失败：" + err, "danger"));
    });
  }
  if (els.bannerAction) {
    els.bannerAction.addEventListener("click", () => {
      const id = els.bannerAction.dataset.action;
      if (id === "start-gui") {
        runPrimary({ forceLaunch: true }).catch(() => {});
      } else if (id === "retry-detect") {
        detect().catch(() => {});
      } else if (id === "copy-install-command") {
        copyInstallCommand().catch(() => {});
      } else if (id === "enable") {
        setEnabled(true).catch(() => {});
      }
    });
  }
  if (els.settings) {
    // 唯一设置入口：头部齿轮（页脚已移除，版本/帮助只在 options 页）。
    els.settings.addEventListener("click", () => openOptions());
  }
  if (els.recentClear) {
    els.recentClear.addEventListener("click", () => {
      clearRecent().catch(() => {});
    });
  }
  if (els.undoAction) {
    els.undoAction.addEventListener("click", () => {
      undoClear().catch(() => {});
    });
  }
  if (els.activeOpen) {
    els.activeOpen.addEventListener("click", () => {
      openRdm().catch(() => {});
    });
  }
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function init() {
  bindEvents();
  setHidden(els.banner, true);
  setHidden(els.undoToast, true);
  setHidden(els.activeSection, true);
  renderPrimary();

  const initial = await send({ type: "getState" });
  if (initial && initial.ok === true) {
    applyState(initial);
  } else {
    showResult("后台无响应，请在设置中重新检测", "danger");
  }

  const context = await send({ type: "getTabContext" });
  if (context && context.ok === true) {
    tabContext = context;
  } else {
    tabContext = { url: "", title: "", selectedUrls: [] };
  }
  renderPrimary();

  await detect().catch(() => {});
  startPolling();
}

window.addEventListener("unload", cleanup);
window.addEventListener("pagehide", cleanup);

init().catch((err) => {
  showResult("popup 初始化失败：" + err, "danger");
});
