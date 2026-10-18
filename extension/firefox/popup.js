/*!
 * RDM 下载接管 — popup 逻辑。
 * 只负责渲染后台 getState/testConnection 返回的快照；所有决策在 background.js。
 */
"use strict";

/* global RDMPure */
const { formatAge } = RDMPure;

const STATUS_LABELS = {
  handed: "已转交",
  queued: "已排队",
  fallback: "已回退",
  error: "失败",
};

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
};

let currentState = null;
let ageTimer = null;

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

function renderRecent(state) {
  const list = (state && state.recent) || [];
  els.recentList.textContent = "";
  if (!Array.isArray(list) || list.length === 0) {
    const empty = document.createElement("li");
    empty.className = "empty";
    empty.textContent = "暂无记录";
    els.recentList.appendChild(empty);
    return;
  }
  for (const entry of list) {
    if (!entry || typeof entry !== "object") continue;
    const item = document.createElement("li");
    item.className = "item";

    const top = document.createElement("div");
    top.className = "item-top";

    const name = document.createElement("span");
    name.className = "item-name";
    name.textContent = entry.fileName || "（未知文件）";
    name.title = entry.url || "";

    const badge = document.createElement("span");
    const statusKey = STATUS_LABELS[entry.status] ? entry.status : "error";
    badge.className = "badge " + statusKey;
    badge.textContent = STATUS_LABELS[statusKey];

    top.appendChild(name);
    top.appendChild(badge);

    const meta = document.createElement("div");
    meta.className = "item-meta";
    const url = document.createElement("span");
    url.className = "item-url";
    url.textContent = entry.url || "";
    url.title = entry.url || "";
    const age = document.createElement("span");
    age.dataset.at = String(entry.at || 0);
    age.textContent = formatAge(entry.at);
    meta.appendChild(url);
    meta.appendChild(age);

    item.appendChild(top);
    item.appendChild(meta);
    if (entry.detail) {
      const detail = document.createElement("div");
      detail.className = "item-detail";
      detail.textContent = entry.detail;
      item.appendChild(detail);
    }
    els.recentList.appendChild(item);
  }
}

function render(state) {
  if (!state || !state.ok) {
    renderStatus({ status: { host: "error", detail: (state && state.error) || "后台无响应" } });
    return;
  }
  currentState = state;
  if (state.settings) {
    els.enabled.checked = state.settings.enabled !== false;
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
}

async function init() {
  bindEvents();
  await loadState();
  await detect();
  ageTimer = setInterval(refreshAges, 1000);
}

window.addEventListener("unload", () => {
  if (ageTimer) clearInterval(ageTimer);
});

init().catch((err) => {
  renderStatus({ status: { host: "error", detail: "popup 初始化失败：" + err } });
});
