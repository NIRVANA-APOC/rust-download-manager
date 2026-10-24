/*!
 * options 页回归测试（node:test + node:vm + 手写最小 DOM/browser stub）。
 *
 * 覆盖 M4 重做后的关键行为：
 *   - 分组结构与 720px 内容区约束、无「保存设置」按钮；
 *   - 改动即保存（400ms 防抖）、字段级校验（非法值不写入）；
 *   - 「测试连接」给人话结果（宿主 / GUI / 延迟）；
 *   - 首次安装引导、Cookie 首次确认（始终/仅本次/取消）；
 *   - 恢复本组默认、复制诊断信息、Chrome 实验性标识。
 *
 * 运行：node --test extension/firefox/test/
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const pure = require("../lib/pure.js");

const ROOT = path.resolve(__dirname, "..");
const OPTIONS_HTML = fs.readFileSync(path.join(ROOT, "options.html"), "utf8");
const OPTIONS_CSS = fs.readFileSync(path.join(ROOT, "options.css"), "utf8");
const TOKENS_CSS = fs.readFileSync(path.join(ROOT, "lib", "tokens.css"), "utf8");
const PURE_JS = fs.readFileSync(path.join(ROOT, "lib", "pure.js"), "utf8");
const OPTIONS_JS = fs.readFileSync(path.join(ROOT, "options.js"), "utf8");

const HTML_IDS = [...OPTIONS_HTML.matchAll(/id="([^"]+)"/g)].map((match) => match[1]);
const AUTO_SAVE_MS = 400;

const REQUIRED_IDS = [
  "version",
  "chrome-badge",
  "save-status",
  "status-card",
  "status-dot",
  "status-headline",
  "status-detail",
  "status-browser",
  "status-time",
  "test",
  "copy-diagnostics",
  "onboarding",
  "onboarding-steps",
  "onboarding-dismiss",
  "enabled",
  "auto-start-gui",
  "context-menu",
  "page-toast",
  "min-size-mb",
  "allow-extensions",
  "block-extensions",
  "output-dir",
  "speed-limit",
  "forward-cookies",
  "ai-credentials",
  "err-min-size",
  "err-allow-extensions",
  "err-block-extensions",
  "err-output-dir",
  "err-speed-limit",
  "cookie-confirm",
  "cookie-allow-once",
  "cookie-allow-always",
  "cookie-cancel",
  "diagnostics",
  "diag-host",
  "diag-gui",
  "diag-checked",
  "diag-error",
  "diag-version",
  "diag-test",
  "diag-copy",
  "install-firefox",
  "firefox-command",
  "install-chrome",
  "chrome-command",
  "copy-firefox-command",
  "copy-chrome-command",
  "diagnostic-text",
  "about-version",
  "about-browser",
  "save-toast",
  "reset-basic",
  "reset-rules",
  "reset-behavior",
  "reset-privacy",
];

// ---------------------------------------------------------------------------
// 最小 DOM stub
// ---------------------------------------------------------------------------

class ClassList {
  constructor() {
    this.set = new Set();
  }
  add(...names) {
    for (const name of names) this.set.add(name);
  }
  remove(...names) {
    for (const name of names) this.set.delete(name);
  }
  contains(name) {
    return this.set.has(name);
  }
  toggle(name, force) {
    const on = force === undefined ? !this.set.has(name) : force === true;
    if (on) this.set.add(name);
    else this.set.delete(name);
    return on;
  }
}

class El {
  constructor(tag) {
    this.tagName = String(tag || "div").toUpperCase();
    this.children = [];
    this.parentNode = null;
    this.attributes = Object.create(null);
    this.dataset = {};
    this.style = {};
    this.hidden = false;
    this.checked = false;
    this.disabled = false;
    this.open = false;
    this.value = "";
    this.type = "";
    this.title = "";
    this.classList = new ClassList();
    this._className = "";
    this._text = "";
    this._listeners = Object.create(null);
  }
  get className() {
    return this._className;
  }
  set className(value) {
    this._className = value === null || value === undefined ? "" : String(value);
    this.classList.set = new Set(this._className.split(/\s+/).filter(Boolean));
  }
  get textContent() {
    return this._text;
  }
  set textContent(value) {
    this._text = value === null || value === undefined ? "" : String(value);
    this.children = [];
  }
  appendChild(child) {
    this.children.push(child);
    child.parentNode = this;
    return child;
  }
  setAttribute(name, value) {
    this.attributes[name] = String(value);
  }
  removeAttribute(name) {
    delete this.attributes[name];
  }
  hasAttribute(name) {
    return name in this.attributes;
  }
  getAttribute(name) {
    return name in this.attributes ? this.attributes[name] : null;
  }
  addEventListener(type, fn) {
    (this._listeners[type] || (this._listeners[type] = [])).push(fn);
  }
  dispatch(type) {
    for (const fn of this._listeners[type] || []) fn({});
  }
  scrollIntoView() {}
}

function createStorage() {
  const data = new Map();
  return {
    getItem: (key) => (data.has(key) ? data.get(key) : null),
    setItem: (key, value) => {
      data.set(key, String(value));
    },
    removeItem: (key) => data.delete(key),
    data,
  };
}

function baseState(overrides) {
  const opts = overrides || {};
  return {
    ok: true,
    settings: pure.normalizeSettings(opts.settings),
    status: Object.assign(
      {
        host: "ok",
        gui: true,
        hostVersion: "0.2.0",
        detail: "",
        checkedAt: Date.now(),
      },
      opts.status || {}
    ),
    recent: [],
    extensionVersion: "0.2.0",
    nativeHost: "com.rdm.download",
    browser: opts.browser || "firefox",
    activity: { active: 0, downloading: 0, paused: 0, failed: 0, speedBps: 0, updatedAt: 0 },
  };
}

function createHarness(options) {
  const opts = options && typeof options === "object" ? options : {};
  const els = {};
  for (const id of HTML_IDS) els[id] = new El("div");

  const lifecycle = { pagehide: [], unload: [] };
  const timeouts = new Map();
  let nextTimerId = 1;

  const harness = {
    els,
    lifecycle,
    timeouts,
    sentMessages: [],
    clipboard: [],
    localStorage: createStorage(),
    sessionStorage: createStorage(),
    state: opts.state || baseState(),
    tabCreates: [],
  };

  harness.handleMessage = (message) => {
    if (typeof harness[message.type] === "function") return harness[message.type](message);
    if (message.type === "getState") return Promise.resolve(harness.state);
    if (message.type === "testConnection") {
      return Promise.resolve(
        Object.assign({}, harness.state, { latencyMs: 20, status: harness.state.status })
      );
    }
    if (message.type === "setSettings") {
      harness.state = Object.assign({}, harness.state, {
        settings: pure.normalizeSettings(
          Object.assign({}, harness.state.settings, message.settings)
        ),
      });
      return Promise.resolve(Object.assign({}, harness.state));
    }
    if (message.type === "openOptions") return Promise.resolve({ ok: true });
    return Promise.resolve({ ok: false, error: "unknown_message_type" });
  };

  for (const key of ["testConnection", "setSettings"]) {
    if (typeof opts[key] === "function") harness[key] = opts[key];
  }

  const document = {
    getElementById: (id) => els[id] || null,
    createElement: (tag) => new El(tag),
  };
  const window = {
    addEventListener: (type, fn) => {
      (lifecycle[type] || (lifecycle[type] = [])).push(fn);
    },
  };
  const navigator = {
    clipboard: {
      writeText: (text) => {
        harness.clipboard.push(text);
        return Promise.resolve();
      },
    },
  };
  const location = { hash: "" };
  const browser = {
    runtime: {
      id: "rdm@rdm.download",
      sendMessage: (message) => {
        harness.sentMessages.push(message);
        return Promise.resolve()
          .then(() => harness.handleMessage(message))
          .catch((err) => ({ ok: false, error: String(err) }));
      },
      openOptionsPage: () => Promise.resolve(),
      getURL: (relative) => "moz-extension://test/" + relative,
    },
    tabs: {
      create: (createOptions) => {
        harness.tabCreates.push(createOptions);
        return Promise.resolve();
      },
    },
  };

  const sandbox = {
    document,
    window,
    navigator,
    location,
    browser,
    console,
    URL,
    TextEncoder,
    localStorage: harness.localStorage,
    sessionStorage: harness.sessionStorage,
    setTimeout: (fn, ms) => {
      const id = nextTimerId++;
      timeouts.set(id, { id, fn, ms });
      return id;
    },
    clearTimeout: (id) => {
      timeouts.delete(id);
    },
  };

  const context = vm.createContext(sandbox);
  vm.runInContext(PURE_JS, context, { filename: path.join(ROOT, "lib", "pure.js") });
  vm.runInContext(OPTIONS_JS, context, { filename: path.join(ROOT, "options.js") });

  harness.settle = async () => {
    for (let index = 0; index < 12; index += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  };
  harness.fireTimeouts = (ms) => {
    const due = [];
    for (const entry of Array.from(timeouts.values())) {
      if (ms === undefined || entry.ms === ms) due.push(entry);
    }
    for (const entry of due) {
      timeouts.delete(entry.id);
      entry.fn();
    }
  };
  harness.setValue = (id, value) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.value = String(value);
    element.dispatch("input");
  };
  harness.toggle = (id, checked) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.checked = checked === undefined ? !element.checked : checked;
    element.dispatch("change");
  };
  harness.click = (id) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.dispatch("click");
  };
  harness.lifecycleFire = (type) => {
    for (const fn of lifecycle[type] || []) fn();
  };
  harness.savedSettings = () => {
    const messages = harness.sentMessages.filter((message) => message.type === "setSettings");
    return messages.length > 0 ? messages[messages.length - 1].settings : null;
  };
  return harness;
}

async function boot(options) {
  const harness = createHarness(options);
  await harness.settle();
  return harness;
}

// ---------------------------------------------------------------------------
// 1) 结构 / 分组 / 无保存按钮
// ---------------------------------------------------------------------------

test("options.html：分组齐全、tokens.css、内容区 ≤720、无「保存设置」按钮", () => {
  for (const id of REQUIRED_IDS) {
    assert.ok(HTML_IDS.includes(id), "options.html 缺少 id: #" + id);
  }
  assert.ok(OPTIONS_HTML.includes('href="lib/tokens.css"'));
  assert.ok(OPTIONS_CSS.includes("--options-content-width"), "内容区宽度必须走 token");
  assert.match(TOKENS_CSS, /--options-content-width: 720px/);
  assert.equal(/id="save"/.test(OPTIONS_HTML), false, "新版不得再有「保存设置」按钮");
  assert.equal(/<button[^>]*>保存设置<\/button>/.test(OPTIONS_HTML), false, "新版不得再有保存按钮");
  assert.equal(OPTIONS_HTML.includes("AI 解析"), false);
  for (const title of ["基础", "接管规则", "下载行为", "隐私与权限", "连接诊断", "关于"]) {
    assert.ok(OPTIONS_HTML.includes(title), "缺少分组：" + title);
  }
  assert.ok(OPTIONS_HTML.indexOf("lib/pure.js") < OPTIONS_HTML.indexOf("options.js"));
});

// ---------------------------------------------------------------------------
// 2) 加载设置（key 兼容）
// ---------------------------------------------------------------------------

test("加载：所有既有设置项回填且版本/浏览器正确", async () => {
  const harness = await boot({
    state: baseState({
      settings: {
        enabled: false,
        autoStartGui: false,
        contextMenu: false,
        pageToast: true,
        minSizeBytes: 5 * 1024 * 1024,
        allowExtensions: ["iso", "zip"],
        blockExtensions: ["html"],
        outputDir: "D:\\Downloads",
        speedLimitKiBps: 256,
        forwardCookies: false,
        aiIncludeCredentials: true,
      },
    }),
  });
  assert.equal(harness.els["enabled"].checked, false);
  assert.equal(harness.els["auto-start-gui"].checked, false);
  assert.equal(harness.els["context-menu"].checked, false);
  assert.equal(harness.els["page-toast"].checked, true);
  assert.equal(harness.els["min-size-mb"].value, "5");
  assert.equal(harness.els["allow-extensions"].value, "iso, zip");
  assert.equal(harness.els["block-extensions"].value, "html");
  assert.equal(harness.els["output-dir"].value, "D:\\Downloads");
  assert.equal(harness.els["speed-limit"].value, "256");
  assert.equal(harness.els["forward-cookies"].checked, false);
  assert.equal(harness.els["ai-credentials"].checked, true);
  assert.equal(harness.els["version"].textContent, "扩展 v0.2.0");
  assert.equal(harness.els["chrome-badge"].hidden, true);
  assert.match(harness.els["status-headline"].textContent, /原生宿主已连接（v0\.2\.0） · RDM 运行中/);
});

// ---------------------------------------------------------------------------
// 3) 自动保存（400ms 防抖）
// ---------------------------------------------------------------------------

test("自动保存：输入后 400ms 防抖，只发一次 setSettings", async () => {
  const harness = await boot();
  harness.setValue("min-size-mb", "3");
  harness.setValue("min-size-mb", "5");
  assert.equal(harness.els["save-status"].textContent, "保存中…");
  assert.equal(
    harness.sentMessages.filter((message) => message.type === "setSettings").length,
    0,
    "防抖窗口内不得保存"
  );
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  const sent = harness.savedSettings();
  assert.ok(sent);
  assert.equal(sent.minSizeBytes, 5 * 1024 * 1024);
  assert.equal(harness.els["save-status"].textContent, "已保存");
  assert.equal(harness.els["save-status"].className, "save-status ok");
});

test("字段校验：相对保存目录不写入，其他字段仍保存；修正后恢复", async () => {
  const harness = await boot({
    state: baseState({ settings: { outputDir: "D:\\Downloads" } }),
  });
  harness.setValue("output-dir", "relative\\dir");
  harness.setValue("min-size-mb", "7");
  assert.equal(harness.els["err-output-dir"].hidden, false);
  assert.match(harness.els["err-output-dir"].textContent, /必须是绝对路径/);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  const sent = harness.savedSettings();
  assert.ok(sent);
  assert.equal(sent.outputDir, "D:\\Downloads", "非法路径必须保留上一次有效值");
  assert.equal(sent.minSizeBytes, 7 * 1024 * 1024, "其他合法字段仍应保存");

  harness.setValue("output-dir", "E:\\Downloads");
  assert.equal(harness.els["err-output-dir"].hidden, true);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.equal(harness.savedSettings().outputDir, "E:\\Downloads");
});

test("字段校验：非法扩展名 token 不写入，行内红字提示", async () => {
  const harness = await boot();
  harness.setValue("allow-extensions", "iso, bad/ext");
  assert.equal(harness.els["err-allow-extensions"].hidden, false);
  assert.match(harness.els["err-allow-extensions"].textContent, /扩展名/);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.deepEqual(Array.from(harness.savedSettings().allowExtensions), []);

  harness.setValue("allow-extensions", "iso, .ZIP");
  assert.equal(harness.els["err-allow-extensions"].hidden, true);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.deepEqual(Array.from(harness.savedSettings().allowExtensions), ["iso", "zip"]);
});

test("字段校验：负数限速不写入", async () => {
  const harness = await boot();
  harness.setValue("speed-limit", "-5");
  assert.equal(harness.els["err-speed-limit"].hidden, false);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.equal(harness.savedSettings().speedLimitKiBps, 0);
});

test("翻页关闭时把防抖中的修改落盘（不丢改动）", async () => {
  const harness = await boot();
  harness.setValue("min-size-mb", "9");
  assert.equal(
    harness.sentMessages.filter((message) => message.type === "setSettings").length,
    0
  );
  harness.lifecycleFire("pagehide");
  await harness.settle();
  const sent = harness.sentMessages.find((message) => message.type === "setSettings");
  assert.ok(sent, "pagehide 必须 flush 待保存改动");
  assert.equal(sent.settings.minSizeBytes, 9 * 1024 * 1024);
});

// ---------------------------------------------------------------------------
// 4) 测试连接 / 诊断
// ---------------------------------------------------------------------------

test("测试连接：给人话结果（宿主已连接、GUI 运行中、延迟）", async () => {
  const harness = await boot({
    state: baseState({ status: { host: "missing", gui: false } }),
    testConnection: () =>
      Promise.resolve(
        baseState({
          status: { host: "ok", gui: true, hostVersion: "0.2.0", checkedAt: Date.now() },
        })
      ),
  });
  harness.click("test");
  await harness.settle();
  assert.match(harness.els["status-headline"].textContent, /原生宿主已连接（v0\.2\.0）/);
  assert.match(harness.els["status-headline"].textContent, /RDM 运行中/);
  assert.match(harness.els["status-detail"].textContent, /延迟 \d+ ms/);
  assert.equal(harness.els["status-card"].className, "card status-card tone-ok");
  assert.equal(harness.els["test"].disabled, false);
});

test("复制诊断信息：脱敏、不含 URL", async () => {
  const harness = await boot({
    state: baseState({
      status: { host: "error", gui: false, detail: "invalid_request", checkedAt: Date.now() },
    }),
  });
  harness.click("copy-diagnostics");
  await harness.settle();
  assert.equal(harness.clipboard.length, 1);
  const text = harness.clipboard[0];
  assert.match(text, /RDM 浏览器接管诊断/);
  assert.match(text, /原生宿主：连接异常/);
  assert.match(text, /最近错误：invalid_request/);
  assert.equal(text.includes("http"), false);
  assert.equal(text.includes("Cookie"), false);
});

// ---------------------------------------------------------------------------
// 5) 首次安装引导
// ---------------------------------------------------------------------------

test("首次安装引导：宿主未安装显示 3 步，复制命令、可收起", async () => {
  const harness = await boot({
    state: baseState({ status: { host: "missing", gui: false } }),
  });
  assert.equal(harness.els["onboarding"].hidden, false);
  const steps = harness.els["onboarding-steps"].children;
  assert.equal(steps.length, 3);
  assert.equal(steps[0].className, "step todo");
  assert.equal(
    steps[0].children[2].textContent,
    "待完成"
  );
  const action = steps[0].children[3];
  assert.equal(action.textContent, "复制安装命令");
  action.dispatch("click");
  await harness.settle();
  assert.match(harness.clipboard[0], /install-firefox-host\.ps1/);

  harness.click("onboarding-dismiss");
  assert.equal(harness.els["onboarding"].hidden, true);
  assert.equal(harness.localStorage.getItem("rdm.onboarding.dismissed"), "1");
});

test("Chrome：实验性标签 + Chrome 专属安装命令，不显示 Firefox 步骤", async () => {
  const harness = await boot({ state: baseState({ browser: "chrome" }) });
  assert.equal(harness.els["chrome-badge"].hidden, false);
  assert.equal(harness.els["about-browser"].textContent, "Chrome（实验性）");
  assert.equal(harness.els["install-firefox"].hidden, true);
  assert.equal(harness.els["install-chrome"].hidden, false);
  assert.match(harness.els["chrome-command"].textContent, /install-chrome-host\.ps1/);
});

// ---------------------------------------------------------------------------
// 6) Cookie 首次确认
// ---------------------------------------------------------------------------

test("Cookie 首次开启：轻量确认卡；始终允许后写入设置", async () => {
  const harness = await boot({
    state: baseState({ settings: { forwardCookies: false } }),
  });
  assert.equal(harness.els["forward-cookies"].checked, false);
  harness.toggle("forward-cookies", true);
  await harness.settle();
  assert.equal(harness.els["cookie-confirm"].hidden, false);
  assert.equal(harness.els["forward-cookies"].checked, false, "确认前不得直接开启");
  assert.equal(
    harness.sentMessages.filter((message) => message.type === "setSettings").length,
    0
  );

  harness.click("cookie-allow-always");
  assert.equal(harness.localStorage.getItem("rdm.privacy.confirmed"), "always");
  assert.equal(harness.els["cookie-confirm"].hidden, true);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.equal(harness.savedSettings().forwardCookies, true);

  // 已确认后再次切换不再弹卡。
  harness.toggle("forward-cookies", false);
  await harness.settle();
  assert.equal(harness.els["cookie-confirm"].hidden, true);
});

test("Cookie 首次开启：仅本次允许在页面关闭时恢复关闭；取消不保存", async () => {
  const once = await boot({
    state: baseState({ settings: { forwardCookies: false } }),
  });
  once.toggle("forward-cookies", true);
  once.click("cookie-allow-once");
  once.fireTimeouts(AUTO_SAVE_MS);
  await once.settle();
  assert.equal(once.savedSettings().forwardCookies, true);
  once.lifecycleFire("pagehide");
  await once.settle();
  const restore = once.sentMessages
    .filter((message) => message.type === "setSettings")
    .pop();
  assert.equal(restore.settings.forwardCookies, false, "仅本次允许必须在关闭时恢复");

  const cancel = await boot({
    state: baseState({ settings: { forwardCookies: false } }),
  });
  cancel.toggle("forward-cookies", true);
  cancel.click("cookie-cancel");
  cancel.fireTimeouts(AUTO_SAVE_MS);
  await cancel.settle();
  assert.equal(cancel.els["cookie-confirm"].hidden, true);
  assert.equal(cancel.els["forward-cookies"].checked, false);
  assert.equal(
    cancel.sentMessages.filter((message) => message.type === "setSettings").length,
    0
  );
});

// ---------------------------------------------------------------------------
// 7) 恢复本组默认 / 新开关
// ---------------------------------------------------------------------------

test("恢复本组默认：基础组回到默认值并自动保存", async () => {
  const harness = await boot({
    state: baseState({ settings: { enabled: false, autoStartGui: false } }),
  });
  assert.equal(harness.els["enabled"].checked, false);
  harness.click("reset-basic");
  assert.equal(harness.els["enabled"].checked, true);
  assert.equal(harness.els["auto-start-gui"].checked, true);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  const sent = harness.savedSettings();
  assert.equal(sent.enabled, true);
  assert.equal(sent.autoStartGui, true);
});

test("页面内接管提示开关：默认关闭，可开启并自动保存", async () => {
  const harness = await boot();
  assert.equal(harness.els["page-toast"].checked, false);
  harness.toggle("page-toast", true);
  harness.fireTimeouts(AUTO_SAVE_MS);
  await harness.settle();
  assert.equal(harness.savedSettings().pageToast, true);
});
