/*!
 * RDM 下载接管 — 选项页逻辑。
 * 表单 <-> settings 的转换全部走 lib/pure.js 的纯函数，便于 Node 单测覆盖。
 */
"use strict";

/* global RDMPure */
const {
  DEFAULT_SETTINGS,
  normalizeSettings,
  parseExtensionList,
  formatExtensionList,
  parseMegabytesToBytes,
  formatMegabytesFromBytes,
} = RDMPure;

const els = {
  form: document.getElementById("settings-form"),
  enabled: document.getElementById("enabled"),
  autoStartGui: document.getElementById("auto-start-gui"),
  contextMenu: document.getElementById("context-menu"),
  forwardCookies: document.getElementById("forward-cookies"),
  aiCredentials: document.getElementById("ai-credentials"),
  minSizeMb: document.getElementById("min-size-mb"),
  allowExtensions: document.getElementById("allow-extensions"),
  blockExtensions: document.getElementById("block-extensions"),
  outputDir: document.getElementById("output-dir"),
  speedLimit: document.getElementById("speed-limit"),
  save: document.getElementById("save"),
  reset: document.getElementById("reset"),
  test: document.getElementById("test"),
  toast: document.getElementById("toast"),
  version: document.getElementById("version"),
  debugLine: document.getElementById("debug-line"),
};

let toastTimer = null;

function send(message) {
  return browser.runtime.sendMessage(message).catch((err) => ({
    ok: false,
    error: String(err),
  }));
}

function showToast(text, kind) {
  els.toast.textContent = text;
  els.toast.className = "toast" + (kind ? " " + kind : "");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    els.toast.textContent = "";
    els.toast.className = "toast";
  }, 4000);
}

function fillForm(settings) {
  const normalized = normalizeSettings(settings);
  els.enabled.checked = normalized.enabled;
  els.autoStartGui.checked = normalized.autoStartGui;
  els.contextMenu.checked = normalized.contextMenu;
  els.forwardCookies.checked = normalized.forwardCookies;
  els.aiCredentials.checked = normalized.aiIncludeCredentials;
  els.minSizeMb.value = formatMegabytesFromBytes(normalized.minSizeBytes);
  els.allowExtensions.value = formatExtensionList(normalized.allowExtensions);
  els.blockExtensions.value = formatExtensionList(normalized.blockExtensions);
  els.outputDir.value = normalized.outputDir;
  els.speedLimit.value = normalized.speedLimitKiBps > 0 ? String(normalized.speedLimitKiBps) : "";
}

function readForm() {
  const outputDir = els.outputDir.value.trim();
  return normalizeSettings({
    enabled: els.enabled.checked,
    autoStartGui: els.autoStartGui.checked,
    contextMenu: els.contextMenu.checked,
    forwardCookies: els.forwardCookies.checked,
    aiIncludeCredentials: els.aiCredentials.checked,
    minSizeBytes: parseMegabytesToBytes(els.minSizeMb.value),
    allowExtensions: parseExtensionList(els.allowExtensions.value),
    blockExtensions: parseExtensionList(els.blockExtensions.value),
    outputDir,
    speedLimitKiBps: parseInt(els.speedLimit.value, 10) || 0,
  });
}

async function load() {
  const state = await send({ type: "getState" });
  if (!state || !state.ok) {
    showToast("无法读取设置：" + ((state && state.error) || "后台无响应"), "bad");
    return;
  }
  fillForm(state.settings);
  if (state.extensionVersion) {
    els.version.textContent = "（扩展 v" + state.extensionVersion + "）";
  }
  const status = state.status || {};
  const hostLabel =
    status.host === "ok"
      ? "原生宿主已连接" + (status.hostVersion ? "（v" + status.hostVersion + "）" : "")
      : status.host === "missing"
        ? "原生宿主未安装"
        : status.host === "error"
          ? "原生宿主连接异常"
          : "原生宿主状态未知";
  els.debugLine.textContent =
    hostLabel + "；GUI：" + (status.gui ? "运行中" : "未运行") + (status.detail ? "；" + status.detail : "");
}

async function save() {
  const settings = readForm();
  if (settings.outputDir && !/^[a-zA-Z]:[\\/]/.test(settings.outputDir) && !settings.outputDir.startsWith("\\\\")) {
    showToast("保存目录必须是绝对路径（例如 D:\\Downloads）", "bad");
    return;
  }
  const state = await send({ type: "setSettings", settings });
  if (state && state.ok) {
    fillForm(state.settings);
    showToast("设置已保存", "ok");
  } else {
    showToast("保存失败：" + ((state && state.error) || "未知错误"), "bad");
  }
}

async function testConnection() {
  els.test.disabled = true;
  els.test.textContent = "检测中…";
  try {
    const state = await send({ type: "testConnection" });
    if (state && state.ok) {
      const status = state.status || {};
      if (status.host === "ok") {
        showToast("原生宿主已连接；RDM GUI " + (status.gui ? "运行中" : "未运行（下载会排队）"), "ok");
      } else {
        showToast("连接失败：" + (status.detail || status.host), "bad");
      }
      await load();
    } else {
      showToast("连接失败：" + ((state && state.error) || "后台无响应"), "bad");
    }
  } finally {
    els.test.disabled = false;
    els.test.textContent = "测试连接";
  }
}

function bindEvents() {
  els.form.addEventListener("submit", (event) => {
    event.preventDefault();
    save().catch((err) => showToast("保存失败：" + err, "bad"));
  });

  els.reset.addEventListener("click", async () => {
    fillForm(DEFAULT_SETTINGS);
    const state = await send({ type: "setSettings", settings: DEFAULT_SETTINGS });
    if (state && state.ok) {
      showToast("已恢复默认设置", "ok");
    } else {
      showToast("恢复默认失败：" + ((state && state.error) || "未知错误"), "bad");
    }
  });

  els.test.addEventListener("click", () => {
    testConnection().catch(() => {});
  });
}

bindEvents();
load().catch((err) => showToast("加载设置失败：" + err, "bad"));
