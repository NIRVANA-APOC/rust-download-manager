/*!
 * RDM 浏览器接管设置 — options 逻辑（M4 重做）。
 *
 * 交互（docs/design/07-browser-extension.md §11）：
 *   - 改动即保存：400ms 防抖，无「保存设置」按钮；
 *   - 字段级校验：非法值不写入，其他字段仍正常自动保存；
 *   - 「测试连接」给人话结果（宿主是否安装 / GUI 是否运行 / 延迟）；
 *   - 首次开启 Cookie 转发弹轻量确认卡（始终允许 / 仅本次允许 / 取消）；
 *   - 首次安装引导 3 步，可收起；连接诊断折叠区可复制脱敏诊断文本。
 *
 * 纯逻辑（校验/连接文案/诊断文本/引导步骤）在 lib/pure.js，便于 node 单测。
 */
"use strict";

/* global RDMPure */
const {
  DEFAULT_SETTINGS,
  normalizeSettings,
  formatExtensionList,
  parseMegabytesToBytes,
  formatMegabytesFromBytes,
  validateOutputDir,
  validateMinSizeText,
  validateExtensionField,
  formatConnectionResult,
  formatCheckedTime,
  formatDiagnosticText,
  connectionCommands,
  onboardingSteps,
} = RDMPure;

/** 自动保存防抖（07 §11.2：400ms）。 */
const AUTO_SAVE_MS = 400;
const TOAST_MS = 4000;

const els = {
  version: document.getElementById("version"),
  chromeBadge: document.getElementById("chrome-badge"),
  saveStatus: document.getElementById("save-status"),
  statusCard: document.getElementById("status-card"),
  statusDot: document.getElementById("status-dot"),
  statusHeadline: document.getElementById("status-headline"),
  statusDetail: document.getElementById("status-detail"),
  statusBrowser: document.getElementById("status-browser"),
  statusTime: document.getElementById("status-time"),
  test: document.getElementById("test"),
  copyDiagnostics: document.getElementById("copy-diagnostics"),
  onboarding: document.getElementById("onboarding"),
  onboardingDismiss: document.getElementById("onboarding-dismiss"),
  onboardingSteps: document.getElementById("onboarding-steps"),
  enabled: document.getElementById("enabled"),
  autoStartGui: document.getElementById("auto-start-gui"),
  contextMenu: document.getElementById("context-menu"),
  pageToast: document.getElementById("page-toast"),
  minSizeMb: document.getElementById("min-size-mb"),
  allowExtensions: document.getElementById("allow-extensions"),
  blockExtensions: document.getElementById("block-extensions"),
  outputDir: document.getElementById("output-dir"),
  speedLimit: document.getElementById("speed-limit"),
  forwardCookies: document.getElementById("forward-cookies"),
  aiCredentials: document.getElementById("ai-credentials"),
  errMinSize: document.getElementById("err-min-size"),
  errAllowExtensions: document.getElementById("err-allow-extensions"),
  errBlockExtensions: document.getElementById("err-block-extensions"),
  errOutputDir: document.getElementById("err-output-dir"),
  errSpeedLimit: document.getElementById("err-speed-limit"),
  cookieConfirm: document.getElementById("cookie-confirm"),
  cookieAllowOnce: document.getElementById("cookie-allow-once"),
  cookieAllowAlways: document.getElementById("cookie-allow-always"),
  cookieCancel: document.getElementById("cookie-cancel"),
  diagnostics: document.getElementById("diagnostics"),
  diagHost: document.getElementById("diag-host"),
  diagGui: document.getElementById("diag-gui"),
  diagChecked: document.getElementById("diag-checked"),
  diagError: document.getElementById("diag-error"),
  diagVersion: document.getElementById("diag-version"),
  diagTest: document.getElementById("diag-test"),
  diagCopy: document.getElementById("diag-copy"),
  installFirefox: document.getElementById("install-firefox"),
  firefoxCommand: document.getElementById("firefox-command"),
  firefoxNote: document.getElementById("firefox-note"),
  installChrome: document.getElementById("install-chrome"),
  chromeBuildCommand: document.getElementById("chrome-build-command"),
  chromeCommand: document.getElementById("chrome-command"),
  chromeNote: document.getElementById("chrome-note"),
  copyFirefoxCommand: document.getElementById("copy-firefox-command"),
  copyChromeCommand: document.getElementById("copy-chrome-command"),
  diagnosticText: document.getElementById("diagnostic-text"),
  aboutVersion: document.getElementById("about-version"),
  aboutBrowser: document.getElementById("about-browser"),
  saveToast: document.getElementById("save-toast"),
  resetBasic: document.getElementById("reset-basic"),
  resetRules: document.getElementById("reset-rules"),
  resetBehavior: document.getElementById("reset-behavior"),
  resetPrivacy: document.getElementById("reset-privacy"),
};

/** 最近一次 getState 快照。 */
let state = null;
/** 最近一次成功保存 / 加载后的设置（用于非法字段回退）。 */
let currentSettings = normalizeSettings(DEFAULT_SETTINGS);
/** 最近一次测试连接测得的延迟。 */
let lastLatencyMs = null;
let lastTestAt = 0;

let saveTimer = null;
let saveInFlight = false;
let pendingSave = false;
let toastTimer = null;

/** 待确认的 Cookie 隐私开关（首次开启时非系统对话框确认）。 */
let pendingPrivacy = null;
/** 「仅本次允许」：页面关闭时恢复为关闭，避免跨会话持久化。 */
let sessionPrivacyRestore = null;

const PRIVACY_CONFIRMED_KEY = "rdm.privacy.confirmed";
const ONBOARDING_DISMISSED_KEY = "rdm.onboarding.dismissed";

const GROUP_FIELDS = {
  basic: ["enabled", "autoStartGui", "contextMenu", "pageToast"],
  rules: ["minSizeBytes", "allowExtensions", "blockExtensions"],
  behavior: ["outputDir", "speedLimitKiBps"],
  privacy: ["forwardCookies", "aiIncludeCredentials"],
};

// ---------------------------------------------------------------------------
// 小工具
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

function readLocal(key) {
  try {
    return localStorage.getItem(key);
  } catch (_err) {
    return null;
  }
}

function writeLocal(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (_err) {
    /* 无 localStorage 时忽略：仅影响「不再提示」记忆 */
  }
}

function setSaveStatus(text, tone) {
  if (!els.saveStatus) return;
  els.saveStatus.textContent = text || "";
  els.saveStatus.className = "save-status" + (tone ? " " + tone : "");
}

function showToast(text, tone, ttlMs) {
  if (!els.saveToast) return;
  els.saveToast.textContent = text || "";
  els.saveToast.className = "save-toast show" + (tone ? " " + tone : "");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastTimer = null;
    if (els.saveToast) {
      els.saveToast.textContent = "";
      els.saveToast.className = "save-toast";
    }
  }, typeof ttlMs === "number" ? ttlMs : TOAST_MS);
}

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

function browserKind() {
  return state && (state.browser === "chrome" || state.browser === "firefox")
    ? state.browser
    : "firefox";
}

// ---------------------------------------------------------------------------
// 表单 <-> 设置
// ---------------------------------------------------------------------------

function fillForm(settings) {
  const normalized = normalizeSettings(settings);
  if (els.enabled) els.enabled.checked = normalized.enabled;
  if (els.autoStartGui) els.autoStartGui.checked = normalized.autoStartGui;
  if (els.contextMenu) els.contextMenu.checked = normalized.contextMenu;
  if (els.pageToast) els.pageToast.checked = normalized.pageToast;
  if (els.forwardCookies) els.forwardCookies.checked = normalized.forwardCookies;
  if (els.aiCredentials) els.aiCredentials.checked = normalized.aiIncludeCredentials;
  if (els.minSizeMb) els.minSizeMb.value = formatMegabytesFromBytes(normalized.minSizeBytes);
  if (els.allowExtensions) els.allowExtensions.value = formatExtensionList(normalized.allowExtensions);
  if (els.blockExtensions) els.blockExtensions.value = formatExtensionList(normalized.blockExtensions);
  if (els.outputDir) els.outputDir.value = normalized.outputDir;
  if (els.speedLimit) {
    els.speedLimit.value =
      normalized.speedLimitKiBps > 0 ? String(normalized.speedLimitKiBps) : "";
  }
  clearFieldError(els.minSizeMb, els.errMinSize);
  clearFieldError(els.allowExtensions, els.errAllowExtensions);
  clearFieldError(els.blockExtensions, els.errBlockExtensions);
  clearFieldError(els.outputDir, els.errOutputDir);
  clearFieldError(els.speedLimit, els.errSpeedLimit);
}

function validateSpeedLimitText(value) {
  const text = String(value === undefined || value === null ? "" : value).trim();
  if (!text) return "";
  const parsed = Number(text);
  if (!Number.isFinite(parsed) || parsed < 0) return "请输入不小于 0 的数字（KiB/s）";
  if (parsed > 1024 * 1024) return "数值过大（上限 1048576 KiB/s）";
  return "";
}

/**
 * 读取表单 → 设置。
 * 非法字段保留上一次成功保存的值（不写入），其余字段仍然保存（07 §11.2）。
 */
function readForm() {
  const allowField = validateExtensionField(els.allowExtensions ? els.allowExtensions.value : "");
  const blockField = validateExtensionField(els.blockExtensions ? els.blockExtensions.value : "");
  const errors = {
    minSize: validateMinSizeText(els.minSizeMb ? els.minSizeMb.value : ""),
    allow: allowField.error,
    block: blockField.error,
    outputDir: validateOutputDir(els.outputDir ? els.outputDir.value : ""),
    speedLimit: validateSpeedLimitText(els.speedLimit ? els.speedLimit.value : ""),
  };
  const merged = Object.assign({}, normalizeSettings(currentSettings));
  if (els.enabled) merged.enabled = els.enabled.checked;
  if (els.autoStartGui) merged.autoStartGui = els.autoStartGui.checked;
  if (els.contextMenu) merged.contextMenu = els.contextMenu.checked;
  if (els.pageToast) merged.pageToast = els.pageToast.checked;
  if (els.forwardCookies) merged.forwardCookies = els.forwardCookies.checked;
  if (els.aiCredentials) merged.aiIncludeCredentials = els.aiCredentials.checked;
  if (!errors.minSize) {
    merged.minSizeBytes = parseMegabytesToBytes(els.minSizeMb ? els.minSizeMb.value : "");
  }
  if (!errors.allow) merged.allowExtensions = allowField.list;
  if (!errors.block) merged.blockExtensions = blockField.list;
  if (!errors.outputDir && els.outputDir) merged.outputDir = els.outputDir.value.trim();
  if (!errors.speedLimit) {
    merged.speedLimitKiBps = parseInt(els.speedLimit ? els.speedLimit.value : "", 10) || 0;
  }
  return { settings: normalizeSettings(merged), errors };
}

function showFieldError(input, errorEl, message) {
  if (!input || !errorEl) return;
  if (message) {
    input.classList.add("invalid");
    errorEl.textContent = message;
    errorEl.hidden = false;
  } else {
    input.classList.remove("invalid");
    errorEl.textContent = "";
    errorEl.hidden = true;
  }
}

function clearFieldError(input, errorEl) {
  showFieldError(input, errorEl, "");
}

function showErrors(errors) {
  showFieldError(els.minSizeMb, els.errMinSize, errors.minSize);
  showFieldError(els.allowExtensions, els.errAllowExtensions, errors.allow);
  showFieldError(els.blockExtensions, els.errBlockExtensions, errors.block);
  showFieldError(els.outputDir, els.errOutputDir, errors.outputDir);
  showFieldError(els.speedLimit, els.errSpeedLimit, errors.speedLimit);
}

// ---------------------------------------------------------------------------
// 自动保存（400ms 防抖）
// ---------------------------------------------------------------------------

function scheduleSave() {
  if (saveTimer) clearTimeout(saveTimer);
  setSaveStatus("保存中…", "");
  saveTimer = setTimeout(() => {
    saveTimer = null;
    saveNow().catch(() => {});
  }, AUTO_SAVE_MS);
}

async function saveNow() {
  if (saveInFlight) {
    pendingSave = true;
    return;
  }
  const { settings, errors } = readForm();
  showErrors(errors);
  saveInFlight = true;
  setSaveStatus("保存中…", "");
  const result = await send({ type: "setSettings", settings });
  saveInFlight = false;
  if (result && result.ok === true) {
    currentSettings = normalizeSettings(result.settings);
    state = Object.assign({}, state || {}, {
      ok: true,
      settings: currentSettings,
      status: result.status || (state && state.status) || { host: "unknown", gui: false },
      browser: result.browser || (state && state.browser),
      extensionVersion: result.extensionVersion || (state && state.extensionVersion),
      nativeHost: result.nativeHost || (state && state.nativeHost),
    });
    setSaveStatus("已保存", "ok");
    renderStatusCard();
    renderOnboarding();
    renderDiagnostics();
    renderAbout();
  } else {
    setSaveStatus("保存失败", "danger");
    showToast("保存失败，请重试", "danger");
  }
  if (pendingSave) {
    pendingSave = false;
    scheduleSave();
  }
}

// ---------------------------------------------------------------------------
// 状态卡 / 诊断 / 关于
// ---------------------------------------------------------------------------

function hostLabel(status) {
  if (!status || status.host !== "ok") {
    return status && status.host === "missing"
      ? "未安装"
      : status && status.host === "error"
        ? "连接异常"
        : "未知";
  }
  return "已连接" + (status.hostVersion ? "（v" + status.hostVersion + "）" : "");
}

function renderStatusCard() {
  const status = (state && state.status) || {};
  const result = formatConnectionResult({ status, latencyMs: lastLatencyMs });
  if (els.statusHeadline) els.statusHeadline.textContent = result.headline;
  if (els.statusDetail) els.statusDetail.textContent = result.detail;
  if (els.statusCard) els.statusCard.className = "card status-card tone-" + result.tone;
  if (els.statusDot) {
    els.statusDot.className =
      "dot " +
      (result.tone === "ok"
        ? "ok"
        : result.tone === "warn"
          ? "warn"
          : result.tone === "danger"
            ? "bad"
            : "muted");
  }
  if (els.statusBrowser) {
    els.statusBrowser.textContent =
      browserKind() === "chrome" ? "Chrome（实验性）" : "Firefox";
  }
  if (els.statusTime) els.statusTime.textContent = formatCheckedTime(status.checkedAt);
}

function renderDiagnostics() {
  const status = (state && state.status) || {};
  if (els.diagHost) els.diagHost.textContent = hostLabel(status);
  if (els.diagGui) {
    els.diagGui.textContent =
      status.host === "ok" ? (status.gui ? "运行中" : "未运行") : "不可用";
  }
  if (els.diagChecked) els.diagChecked.textContent = formatCheckedTime(status.checkedAt);
  if (els.diagError) els.diagError.textContent = status.detail || "无";
  if (els.diagVersion) {
    els.diagVersion.textContent = (state && state.extensionVersion) || "未知";
  }

  const commands = connectionCommands(browserKind());
  if (els.installFirefox) els.installFirefox.hidden = browserKind() === "chrome";
  if (els.installChrome) els.installChrome.hidden = browserKind() !== "chrome";
  if (els.firefoxCommand) els.firefoxCommand.textContent = commands.installCommand;
  if (els.firefoxNote) els.firefoxNote.textContent = commands.note;
  if (els.chromeBuildCommand) els.chromeBuildCommand.textContent = commands.buildCommand;
  if (els.chromeCommand) els.chromeCommand.textContent = commands.installCommand;
  if (els.chromeNote) els.chromeNote.textContent = commands.note;

  if (els.diagnosticText) {
    els.diagnosticText.textContent = formatDiagnosticText({
      status,
      extensionVersion: state && state.extensionVersion,
      browser: browserKind(),
      latencyMs: lastLatencyMs,
    });
  }
}

function renderAbout() {
  if (els.aboutVersion) {
    els.aboutVersion.textContent = state && state.extensionVersion
      ? "扩展 v" + state.extensionVersion
      : "扩展版本未知";
  }
  if (els.aboutBrowser) {
    els.aboutBrowser.textContent =
      browserKind() === "chrome" ? "Chrome（实验性）" : "Firefox";
  }
}

// ---------------------------------------------------------------------------
// 首次安装引导
// ---------------------------------------------------------------------------

function renderOnboarding() {
  if (!els.onboarding || !els.onboardingSteps) return;
  const status = (state && state.status) || {};
  const steps = onboardingSteps({ status });
  const dismissed = readLocal(ONBOARDING_DISMISSED_KEY) === "1";
  const allDone = steps.every((step) => step.status === "done");
  els.onboarding.hidden = dismissed || allDone;
  if (els.onboarding.hidden) return;
  els.onboardingSteps.textContent = "";
  steps.forEach((step, index) => {
    const item = document.createElement("li");
    item.className = "step " + step.status;
    const number = document.createElement("span");
    number.className = "step-index";
    number.textContent = String(index + 1);
    const text = document.createElement("span");
    text.className = "step-text";
    const title = document.createElement("span");
    title.className = "step-title";
    title.textContent = step.title;
    const hint = document.createElement("span");
    hint.className = "step-hint";
    hint.textContent = step.hint;
    text.appendChild(title);
    text.appendChild(hint);
    const stateLabel = document.createElement("span");
    stateLabel.className = "step-state";
    stateLabel.textContent = step.status === "done" ? "已完成" : "待完成";
    item.appendChild(number);
    item.appendChild(text);
    item.appendChild(stateLabel);
    if (step.status !== "done") {
      const action = document.createElement("button");
      action.type = "button";
      action.className = "btn step-action";
      action.textContent =
        step.action === "copy-install-command"
          ? "复制安装命令"
          : step.action === "test-connection"
            ? "测试连接"
            : "查看安装指南";
      action.addEventListener("click", () => {
        if (step.action === "copy-install-command") {
          copyInstallCommand().catch(() => {});
        } else if (step.action === "test-connection") {
          testConnection().catch(() => {});
        } else {
          openDiagnostics();
        }
      });
      item.appendChild(action);
    }
    els.onboardingSteps.appendChild(item);
  });
}

// ---------------------------------------------------------------------------
// 连接 / 剪贴板动作
// ---------------------------------------------------------------------------

async function testConnection() {
  const buttons = [els.test, els.diagTest].filter(Boolean);
  for (const button of buttons) {
    button.disabled = true;
    if (button === els.test) button.textContent = "检测中…";
  }
  const startedAt = Date.now();
  try {
    const result = await send({ type: "testConnection" });
    if (result && result.ok === true) {
      lastLatencyMs =
        typeof result.latencyMs === "number" ? result.latencyMs : Date.now() - startedAt;
      lastTestAt = Date.now();
      if (result.status) {
        state = Object.assign({}, state || {}, {
          ok: true,
          status: result.status,
          browser: result.browser || (state && state.browser),
          extensionVersion: result.extensionVersion || (state && state.extensionVersion),
        });
      }
      renderStatusCard();
      renderDiagnostics();
      renderOnboarding();
      const human = formatConnectionResult({ status: result.status, latencyMs: lastLatencyMs });
      showToast(human.headline + " · " + human.detail, human.tone === "danger" ? "danger" : "ok");
    } else {
      showToast("连接失败：后台无响应", "danger");
    }
  } finally {
    for (const button of buttons) {
      button.disabled = false;
      if (button === els.test) button.textContent = "测试连接";
    }
  }
}

async function copyDiagnosticText() {
  const text = els.diagnosticText
    ? els.diagnosticText.textContent
    : formatDiagnosticText({ status: (state && state.status) || {}, browser: browserKind() });
  const copied = await copyText(text);
  showToast(copied ? "已复制诊断信息" : "复制失败，请手动选择诊断文本", copied ? "ok" : "danger");
}

async function copyInstallCommand() {
  const commands = connectionCommands(browserKind());
  const copied = await copyText(commands.installCommand);
  showToast(copied ? "已复制安装命令" : "复制失败，请手动复制命令", copied ? "ok" : "danger");
}

async function copyCommandFromElement(id) {
  const element = id ? document.getElementById(id) : null;
  const copied = element ? await copyText(element.textContent) : false;
  showToast(copied ? "已复制命令" : "复制失败，请手动复制命令", copied ? "ok" : "danger");
}

// ---------------------------------------------------------------------------
// 隐私确认（首次开启 Cookie 转发）
// ---------------------------------------------------------------------------

function privacyConfirmed() {
  return readLocal(PRIVACY_CONFIRMED_KEY) === "always";
}

function hideCookieConfirm() {
  if (els.cookieConfirm) els.cookieConfirm.hidden = true;
  pendingPrivacy = null;
}

function handlePrivacyToggle(input) {
  if (!input) return;
  if (input.checked === true && !privacyConfirmed()) {
    input.checked = false;
    pendingPrivacy = input;
    if (els.cookieConfirm) els.cookieConfirm.hidden = false;
    return;
  }
  scheduleSave();
}

function applyPendingPrivacy() {
  if (!pendingPrivacy) return;
  pendingPrivacy.checked = true;
  pendingPrivacy = null;
  if (els.cookieConfirm) els.cookieConfirm.hidden = true;
}

// ---------------------------------------------------------------------------
// 恢复本组默认
// ---------------------------------------------------------------------------

function resetGroup(group) {
  const fields = GROUP_FIELDS[group];
  if (!fields) return;
  const defaults = normalizeSettings(DEFAULT_SETTINGS);
  const merged = Object.assign({}, normalizeSettings(currentSettings));
  for (const field of fields) merged[field] = defaults[field];
  currentSettings = normalizeSettings(merged);
  fillForm(currentSettings);
  scheduleSave();
}

// ---------------------------------------------------------------------------
// 状态应用 / 加载
// ---------------------------------------------------------------------------

function applyState(next, options) {
  if (!next || next.ok !== true) return false;
  state = {
    ok: true,
    settings: next.settings,
    status: next.status,
    browser: next.browser,
    extensionVersion: next.extensionVersion,
    nativeHost: next.nativeHost,
    recent: next.recent,
    activity: next.activity,
  };
  currentSettings = normalizeSettings(next.settings);
  if (!options || options.fill !== false) fillForm(currentSettings);
  if (els.version) {
    els.version.textContent = next.extensionVersion ? "扩展 v" + next.extensionVersion : "";
  }
  if (els.chromeBadge) els.chromeBadge.hidden = browserKind() !== "chrome";
  renderStatusCard();
  renderOnboarding();
  renderDiagnostics();
  renderAbout();
  return true;
}

function openDiagnostics() {
  if (els.diagnostics) els.diagnostics.open = true;
  try {
    if (els.diagnostics && typeof els.diagnostics.scrollIntoView === "function") {
      els.diagnostics.scrollIntoView({ block: "start" });
    }
  } catch (_err) {
    /* 无布局环境忽略 */
  }
}

async function load() {
  const result = await send({ type: "getState" });
  if (result && result.ok === true) {
    applyState(result);
  } else {
    setSaveStatus("读取设置失败", "danger");
    showToast("无法读取设置：后台无响应", "danger");
  }
  try {
    if (location.hash && location.hash.indexOf("help") >= 0) {
      openDiagnostics();
    }
  } catch (_err) {
    /* 无 location 环境忽略 */
  }
}

// ---------------------------------------------------------------------------
// 事件绑定
// ---------------------------------------------------------------------------

function onFieldChanged() {
  const { errors } = readForm();
  showErrors(errors);
  scheduleSave();
}

function bindEvents() {
  const checkboxInputs = [
    els.autoStartGui,
    els.contextMenu,
    els.pageToast,
    els.enabled,
  ].filter(Boolean);
  for (const input of checkboxInputs) {
    input.addEventListener("change", () => {
      scheduleSave();
    });
  }
  for (const input of [els.forwardCookies, els.aiCredentials].filter(Boolean)) {
    input.addEventListener("change", () => {
      handlePrivacyToggle(input);
    });
  }
  for (const input of [
    els.minSizeMb,
    els.allowExtensions,
    els.blockExtensions,
    els.outputDir,
    els.speedLimit,
  ].filter(Boolean)) {
    input.addEventListener("input", onFieldChanged);
    input.addEventListener("change", onFieldChanged);
  }

  for (const button of [els.test, els.diagTest].filter(Boolean)) {
    button.addEventListener("click", () => {
      testConnection().catch(() => {});
    });
  }
  for (const button of [els.copyDiagnostics, els.diagCopy].filter(Boolean)) {
    button.addEventListener("click", () => {
      copyDiagnosticText().catch(() => {});
    });
  }
  if (els.copyFirefoxCommand) {
    els.copyFirefoxCommand.addEventListener("click", () => {
      copyCommandFromElement("firefox-command").catch(() => {});
    });
  }
  if (els.copyChromeCommand) {
    els.copyChromeCommand.addEventListener("click", () => {
      copyCommandFromElement("chrome-command").catch(() => {});
    });
  }
  if (els.onboardingDismiss) {
    els.onboardingDismiss.addEventListener("click", () => {
      writeLocal(ONBOARDING_DISMISSED_KEY, "1");
      if (els.onboarding) els.onboarding.hidden = true;
    });
  }
  for (const [button, group] of [
    [els.resetBasic, "basic"],
    [els.resetRules, "rules"],
    [els.resetBehavior, "behavior"],
    [els.resetPrivacy, "privacy"],
  ]) {
    if (button) {
      button.addEventListener("click", () => resetGroup(group));
    }
  }
  if (els.cookieAllowAlways) {
    els.cookieAllowAlways.addEventListener("click", () => {
      writeLocal(PRIVACY_CONFIRMED_KEY, "always");
      applyPendingPrivacy();
      scheduleSave();
    });
  }
  if (els.cookieAllowOnce) {
    els.cookieAllowOnce.addEventListener("click", () => {
      const pending = pendingPrivacy;
      applyPendingPrivacy();
      if (pending) {
        sessionPrivacyRestore = pending;
        scheduleSave();
      }
    });
  }
  if (els.cookieCancel) {
    els.cookieCancel.addEventListener("click", () => {
      hideCookieConfirm();
    });
  }
}

/** 页面关闭：把待保存的改动与「仅本次允许」的临时项落盘/恢复。 */
function flushOnPageHide() {
  if (saveTimer) {
    clearTimeout(saveTimer);
    saveTimer = null;
    const { settings } = readForm();
    send({ type: "setSettings", settings }).catch(() => {});
  }
  if (sessionPrivacyRestore) {
    const key = sessionPrivacyRestore === els.forwardCookies ? "forwardCookies" : "aiIncludeCredentials";
    sessionPrivacyRestore = null;
    send({ type: "setSettings", settings: { [key]: false } }).catch(() => {});
  }
}

window.addEventListener("pagehide", flushOnPageHide);
window.addEventListener("unload", flushOnPageHide);

bindEvents();
load().catch((err) => {
  setSaveStatus("读取设置失败", "danger");
  showToast("加载设置失败：" + err, "danger");
});
