/*!
 * popup UI 回归测试（node:test + node:vm + 手写最小 DOM/browser stub）。
 *
 * 覆盖 M4 重做后的关键行为：
 *   - 420px 结构与「唯一主按钮」；
 *   - 主按钮随 tab 上下文/连接状态变化的文案与点击行为；
 *   - 状态条只在异常时出现；
 *   - 「正在下载」最多 3 条（进度/速度/溢出）；
 *   - 「最近活动」最多 5 条、清空 + 撤销；
 *   - 开关乐观更新失败回滚、防重复提交、unload 清理。
 *
 * 用 node:vm 加载真实 `lib/pure.js` + `popup.js`，不复制业务逻辑；
 * DOM / browser / 定时器全部手写 stub，不引入 npm 依赖。
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
const POPUP_HTML = fs.readFileSync(path.join(ROOT, "popup.html"), "utf8");
const POPUP_CSS = fs.readFileSync(path.join(ROOT, "popup.css"), "utf8");
const TOKENS_CSS = fs.readFileSync(path.join(ROOT, "lib", "tokens.css"), "utf8");
const PURE_JS = fs.readFileSync(path.join(ROOT, "lib", "pure.js"), "utf8");
const POPUP_JS = fs.readFileSync(path.join(ROOT, "popup.js"), "utf8");

const HTML_IDS = [...POPUP_HTML.matchAll(/id="([^"]+)"/g)].map((match) => match[1]);

const REQUIRED_IDS = [
  "enabled",
  "settings",
  "ready",
  "ready-dot",
  "ready-text",
  "chrome-badge",
  "banner",
  "banner-icon",
  "banner-text",
  "banner-action",
  "primary",
  "primary-text",
  "primary-info",
  "primary-result",
  "active-section",
  "active-head",
  "active-title",
  "active-speed",
  "active-open",
  "active-list",
  "active-more",
  "active-hint",
  "recent-title",
  "recent-clear",
  "recent-list",
  "recent-empty",
  "undo-toast",
  "undo-text",
  "undo-action",
];

const MiB = 1024 * 1024;
const POLL_MS = 1000;
const RESULT_TTL_MS = 2500;
const UNDO_TTL_MS = 5000;

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
    this.title = "";
    this.type = "";
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
  dispatch(type, event) {
    for (const fn of this._listeners[type] || []) fn(event || {});
  }
}

// ---------------------------------------------------------------------------
// 默认快照与 harness
// ---------------------------------------------------------------------------

function settings(overrides) {
  return pure.normalizeSettings(overrides || {});
}

function recentEntry(overrides) {
  return Object.assign(
    {
      at: Date.now(),
      fileName: "a.iso",
      url: "https://example.com/a.iso",
      status: "handed",
      detail: "已交给 RDM",
      downloadId: "dl_a",
    },
    overrides || {}
  );
}

function baseState(overrides) {
  const opts = overrides || {};
  return {
    ok: true,
    settings: settings(opts.settings),
    status: Object.assign({ host: "ok", gui: true, detail: "", checkedAt: Date.now() }, opts.status || {}),
    recent: opts.recent || [],
    extensionVersion: "0.2.0",
    nativeHost: "com.rdm.download",
    browser: opts.browser || "firefox",
    activity: null,
  };
}

function tabContext(overrides) {
  return Object.assign(
    {
      ok: true,
      tabId: 7,
      url: "https://example.com/page",
      title: "Example Page",
      fileName: "",
      sizeBytes: -1,
      selectedUrls: [],
      isHttp: true,
    },
    overrides || {}
  );
}

function emptyActivity(overrides) {
  return Object.assign(
    {
      ok: true,
      guiRunning: true,
      active: 0,
      globalSpeedBps: 0,
      downloads: [],
      recent: [],
    },
    overrides || {}
  );
}

function download(overrides) {
  return Object.assign(
    {
      id: "dl_a",
      fileName: "a.iso",
      state: "downloading",
      downloaded: 0,
      total: 0,
      speedBps: 0,
      connections: 8,
      activeConnections: 0,
      targetConnections: 0,
      peakConnections: 0,
    },
    overrides || {}
  );
}

function createHarness(options) {
  const opts = options && typeof options === "object" ? options : {};
  const els = {};
  for (const id of HTML_IDS) els[id] = new El("div");

  const lifecycleListeners = { unload: [], pagehide: [] };
  const intervals = new Map();
  const intervalHistory = [];
  const timeouts = new Map();
  const timeoutHistory = [];
  let nextTimerId = 1;

  const harness = {
    els,
    lifecycleListeners,
    intervals,
    intervalHistory,
    timeouts,
    timeoutHistory,
    sentMessages: [],
    clipboard: [],
    tabCreates: [],
    optionsPageOpens: 0,
    state: opts.state || baseState(),
    tabContext: opts.tabContext || tabContext(),
    // 真实 background 的 getActivity 会带上最近活动与真实 guiRunning；
    // 默认让 stub 与初始 getState 快照保持一致，避免测试被 1s 轮询覆盖。
    activity:
      opts.activity ||
      emptyActivity({
        guiRunning: !opts.state || !opts.state.status || opts.state.status.gui !== false,
        recent: (opts.state && opts.state.recent) || [],
      }),
  };

  const defaultHandlers = {
    getState: () => harness.state,
    getTabContext: () => harness.tabContext,
    testConnection: () => Object.assign({}, harness.state, { latencyMs: 20 }),
    getActivity: () => harness.activity,
    handoff: () => Promise.resolve({ ok: true, message: "已交给 RDM", results: [] }),
    aiParse: () =>
      Promise.resolve({ ok: true, message: "已发送到 RDM，解析结果在桌面端打开" }),
    setSettings: (message) => {
      harness.state = Object.assign({}, harness.state, {
        settings: settings(Object.assign({}, harness.state.settings, message.settings)),
      });
      return Promise.resolve(harness.state);
    },
    clearRecent: () => {
      harness.clearedRecent = harness.state.recent;
      harness.state = Object.assign({}, harness.state, { recent: [] });
      harness.activity = Object.assign({}, harness.activity, { recent: [] });
      return Promise.resolve(Object.assign({}, harness.state, { undone: false }));
    },
    restoreRecent: () => {
      const restored = harness.clearedRecent || [];
      harness.state = Object.assign({}, harness.state, { recent: restored });
      harness.activity = Object.assign({}, harness.activity, { recent: restored });
      return Promise.resolve(Object.assign({}, harness.state, { undone: true }));
    },
    openRdm: () =>
      Promise.resolve({
        ok: false,
        code: "activate_unsupported",
        message: "当前宿主不支持从浏览器唤起 RDM，请在桌面端任务栏打开",
      }),
  };

  for (const key of Object.keys(defaultHandlers)) {
    harness[key] = opts[key] || defaultHandlers[key];
  }

  harness.handleMessage = (message) => {
    const handler = harness[message.type];
    if (typeof handler === "function") return handler(message);
    return Promise.resolve({ ok: false, error: "unknown_message_type" });
  };

  const document = {
    getElementById: (id) => els[id] || null,
    createElement: (tag) => new El(tag),
  };

  const window = {
    addEventListener: (type, fn) => {
      (lifecycleListeners[type] || (lifecycleListeners[type] = [])).push(fn);
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

  const browser = {
    runtime: {
      sendMessage: (message) => {
        harness.sentMessages.push(message);
        return Promise.resolve()
          .then(() => harness.handleMessage(message))
          .catch((err) => ({ ok: false, error: String(err) }));
      },
      openOptionsPage: () => {
        harness.optionsPageOpens += 1;
        return Promise.resolve();
      },
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
    browser,
    console,
    URL,
    TextEncoder,
    setInterval: (fn, ms) => {
      const id = nextTimerId++;
      const entry = { id, fn, ms };
      intervals.set(id, entry);
      intervalHistory.push(entry);
      return id;
    },
    clearInterval: (id) => {
      intervals.delete(id);
    },
    setTimeout: (fn, ms) => {
      const id = nextTimerId++;
      const entry = { id, fn, ms, cleared: false };
      timeouts.set(id, entry);
      timeoutHistory.push(entry);
      return id;
    },
    clearTimeout: (id) => {
      const entry = timeouts.get(id);
      if (entry) entry.cleared = true;
      timeouts.delete(id);
    },
  };

  const context = vm.createContext(sandbox);
  vm.runInContext(PURE_JS, context, { filename: path.join(ROOT, "lib", "pure.js") });
  vm.runInContext(POPUP_JS, context, { filename: path.join(ROOT, "popup.js") });

  harness.fireIntervals = (ms) => {
    for (const entry of Array.from(intervals.values())) {
      if (ms === undefined || entry.ms === ms) entry.fn();
    }
  };
  harness.fireStaleIntervals = () => {
    for (const entry of intervalHistory) entry.fn();
  };
  harness.fireTimeouts = (ms) => {
    const due = [];
    for (const entry of Array.from(timeouts.values())) {
      if (!entry.cleared && (ms === undefined || entry.ms === ms)) due.push(entry);
    }
    for (const entry of due) {
      timeouts.delete(entry.id);
      entry.fn();
    }
  };
  harness.settle = async () => {
    for (let index = 0; index < 12; index += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  };
  harness.click = (id) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.dispatch("click");
  };
  harness.change = (id) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.dispatch("change");
  };
  harness.lifecycle = (type) => {
    for (const fn of lifecycleListeners[type] || []) fn();
  };
  return harness;
}

async function boot(options) {
  const harness = createHarness(options);
  await harness.settle();
  return harness;
}

function recentNames(harness) {
  return harness.els["recent-list"].children.map(
    (row) => row.children[1] && row.children[1].textContent
  );
}

function activeRows(harness) {
  return harness.els["active-list"].children;
}

// ---------------------------------------------------------------------------
// 1) 结构与尺寸约束
// ---------------------------------------------------------------------------

test("popup.html：420px、tokens.css、唯一主按钮、设置入口只在头部齿轮", () => {
  for (const id of REQUIRED_IDS) {
    assert.ok(HTML_IDS.includes(id), "popup.html 缺少 id: #" + id);
  }
  assert.ok(POPUP_HTML.includes('href="lib/tokens.css"'), "popup 必须引用共享 tokens.css");
  assert.ok(TOKENS_CSS.includes("--popup-width: 420px"), "tokens.css 必须定义 420px popup 宽度");
  assert.ok(POPUP_CSS.includes("var(--popup-width)"), "popup.css 必须使用 token 宽度");
  assert.ok(
    POPUP_HTML.indexOf("lib/pure.js") < POPUP_HTML.indexOf("popup.js"),
    "lib/pure.js 必须先于 popup.js 加载"
  );
  const primaryButtons = [...POPUP_HTML.matchAll(/class="primary"/g)];
  assert.equal(primaryButtons.length, 1, "首屏只能有一个蓝色主按钮");
  assert.match(POPUP_HTML, /id="primary"[^>]*class="primary"/);
  // 2026-10-22 冗余移除：页脚整块（设置/帮助/版本号）不再存在；设置只保留头部齿轮。
  assert.ok(HTML_IDS.includes("settings"), "头部齿轮 #settings 必须保留");
  assert.equal(HTML_IDS.some((id) => id.startsWith("foot-")), false, "popup 不应再有页脚元素");
  assert.equal(HTML_IDS.includes("version"), false, "版本号只在 options 页展示");
  assert.equal(POPUP_HTML.includes('class="foot"'), false, "popup 不应再有页脚容器");
  assert.equal(POPUP_JS.includes("foot-settings"), false, "popup.js 不应残留页脚设置监听");
  assert.equal(POPUP_JS.includes("foot-help"), false, "popup.js 不应残留页脚帮助监听");
  assert.equal(POPUP_JS.includes('getElementById("version")'), false, "popup.js 不应残留版本节点");
  // 术语：不再出现「AI 解析」作为按钮/文案。
  assert.equal(POPUP_HTML.includes("AI 解析"), false);
  assert.equal(POPUP_JS.includes("AI 解析"), false);
  // 正常状态条默认隐藏；正常只有弱化「已就绪」。
  assert.match(POPUP_HTML, /id="banner"[^>]*hidden/);
  assert.match(POPUP_CSS, /--hit-target/, "点击目标尺寸应通过 token 约束");
});

test("设置入口：点击头部齿轮打开 options 页（页脚移除后唯一入口）", async () => {
  const harness = await boot();
  harness.click("settings");
  await harness.settle();
  assert.equal(harness.optionsPageOpens, 1, "头部齿轮必须调用 browser.runtime.openOptionsPage");
});

// ---------------------------------------------------------------------------
// 2) 主按钮上下文
// ---------------------------------------------------------------------------

test("主按钮：文件链接 →「用 RDM 下载此文件」，点击发送 handoff", async () => {
  const harness = await boot({
    tabContext: tabContext({
      url: "https://example.com/files/a.iso",
      fileName: "a.iso",
      sizeBytes: 2 * MiB,
    }),
  });
  assert.equal(harness.els["primary-text"].textContent, "用 RDM 下载此文件");
  assert.equal(harness.els["primary-info"].textContent, "a.iso · 2.0 MiB · example.com");
  assert.equal(harness.els["primary"].disabled, false);
  assert.equal(harness.els["ready-text"].textContent, "已就绪");
  assert.equal(harness.els["banner"].hidden, true);

  harness.click("primary");
  await harness.settle();
  const handoff = harness.sentMessages.find((message) => message.type === "handoff");
  assert.ok(handoff, "必须发送 handoff");
  assert.deepEqual(Array.from(handoff.urls), ["https://example.com/files/a.iso"]);
  assert.equal(handoff.fileName, "a.iso");
  assert.equal(handoff.forceLaunch, false);
  assert.equal(harness.els["primary-result"].textContent, "已交给 RDM");
  assert.equal(harness.els["primary-result"].className, "primary-result ok");
});

test("主按钮：普通页面 →「解析本页资源」，点击发送 aiParse", async () => {
  const harness = await boot({ tabContext: tabContext() });
  assert.equal(harness.els["primary-text"].textContent, "解析本页资源");
  harness.click("primary");
  await harness.settle();
  const parse = harness.sentMessages.find((message) => message.type === "aiParse");
  assert.ok(parse, "必须发送 aiParse");
  assert.equal(parse.forceLaunch, false);
  assert.match(harness.els["primary-result"].textContent, /已发送到 RDM/);
});

test("主按钮：选中 1/多个链接 → 对应文案与批量 handoff", async () => {
  const single = await boot({
    tabContext: tabContext({ selectedUrls: ["https://cdn.example.com/a.iso"] }),
  });
  assert.equal(single.els["primary-text"].textContent, "用 RDM 下载选中的链接");

  const multiple = await boot({
    tabContext: tabContext({
      selectedUrls: ["https://cdn.example.com/a.iso", "https://cdn.example.com/b.zip"],
    }),
  });
  assert.equal(multiple.els["primary-text"].textContent, "下载选中的 2 个链接");
  multiple.click("primary");
  await multiple.settle();
  const handoff = multiple.sentMessages.find((message) => message.type === "handoff");
  assert.deepEqual(Array.from(handoff.urls), [
    "https://cdn.example.com/a.iso",
    "https://cdn.example.com/b.zip",
  ]);
});

test("主按钮：宿主未安装 →「安装浏览器接管」，点击复制 Firefox 安装命令", async () => {
  const harness = await boot({
    state: baseState({ status: { host: "missing", gui: false } }),
    tabContext: tabContext({ url: "https://example.com/a.iso" }),
  });
  assert.equal(harness.els["primary-text"].textContent, "安装浏览器接管");
  assert.equal(harness.els["banner"].hidden, false);
  assert.equal(harness.els["banner"].className, "banner danger");
  assert.match(harness.els["banner-text"].textContent, /浏览器接管尚未安装/);
  assert.equal(harness.els["banner-action"].textContent, "复制安装命令");
  harness.click("primary");
  await harness.settle();
  assert.equal(harness.clipboard.length, 1);
  assert.match(harness.clipboard[0], /install-firefox-host\.ps1/);
  assert.equal(harness.els["primary-result"].textContent, "已复制安装命令");
});

test("主按钮：接管关闭 →「开启下载接管」，点击保存开关", async () => {
  const harness = await boot({
    state: baseState({ settings: { enabled: false } }),
    tabContext: tabContext({ url: "https://example.com/a.iso" }),
  });
  assert.equal(harness.els["primary-text"].textContent, "开启下载接管");
  assert.equal(harness.els["banner"].className, "banner info");
  harness.click("primary");
  await harness.settle();
  const saved = harness.sentMessages.find(
    (message) => message.type === "setSettings" && message.settings.enabled === true
  );
  assert.ok(saved, "点击后应保存 enabled=true");
  assert.equal(harness.els["enabled"].checked, true);
});

test("主按钮：GUI 未运行 + 自动启动关闭 →「启动 RDM 并...」并 forceLaunch", async () => {
  const harness = await boot({
    state: baseState({
      settings: { autoStartGui: false },
      status: { host: "ok", gui: false },
    }),
    tabContext: tabContext({ url: "https://example.com/download/a.iso" }),
  });
  assert.equal(harness.els["primary-text"].textContent, "启动 RDM 并下载此文件");
  assert.equal(harness.els["banner"].className, "banner warn");
  assert.match(harness.els["banner-text"].textContent, /RDM 未运行/);
  harness.click("primary");
  await harness.settle();
  const handoff = harness.sentMessages.find((message) => message.type === "handoff");
  assert.equal(handoff.forceLaunch, true);
});

test("主按钮：GUI 未运行 + 自动启动开启 → 保持上下文动作，状态条给「启动 RDM」", async () => {
  const harness = await boot({
    state: baseState({ status: { host: "ok", gui: false } }),
    tabContext: tabContext(),
  });
  assert.equal(harness.els["primary-text"].textContent, "解析本页资源");
  assert.equal(harness.els["banner-action"].textContent, "启动 RDM");
  harness.click("banner-action");
  await harness.settle();
  const parse = harness.sentMessages.find((message) => message.type === "aiParse");
  assert.ok(parse);
  assert.equal(parse.forceLaunch, true, "状态条「启动 RDM」应显式请求启动 GUI");
});

test("主按钮：非 http 页面禁用且提示不支持", async () => {
  const harness = await boot({
    tabContext: tabContext({ url: "about:addons", isHttp: false }),
  });
  assert.equal(harness.els["primary-text"].textContent, "当前页面不支持");
  assert.equal(harness.els["primary"].disabled, true);
});

// ---------------------------------------------------------------------------
// 3) 连接异常 / 状态条
// ---------------------------------------------------------------------------

test("状态条：宿主连接异常 → danger + 重新检测（点击会重试连接）", async () => {
  let calls = 0;
  const harness = await boot({
    state: baseState({ status: { host: "error", gui: false } }),
    testConnection: () => {
      calls += 1;
      // 首次（popup 打开时的自动检测）仍失败；点击「重新检测」后恢复。
      if (calls === 1) {
        return Promise.resolve(baseState({ status: { host: "error", gui: false } }));
      }
      return Promise.resolve(baseState({ status: { host: "ok", gui: true } }));
    },
  });
  assert.equal(harness.els["banner"].className, "banner danger");
  assert.equal(harness.els["banner-action"].textContent, "重新检测");
  harness.activity = emptyActivity({ guiRunning: true, recent: [] });
  const before = calls;
  harness.click("banner-action");
  await harness.settle();
  assert.ok(calls > before, "重新检测必须重新调用 testConnection");
  assert.equal(harness.els["banner"].hidden, true, "恢复后状态条消失");
  assert.equal(harness.els["ready-text"].textContent, "已就绪");
});

test("Chrome 构建显示实验性标签", async () => {
  const harness = await boot({ state: baseState({ browser: "chrome" }) });
  assert.equal(harness.els["chrome-badge"].hidden, false);
});

// ---------------------------------------------------------------------------
// 4) 正在下载
// ---------------------------------------------------------------------------

test("正在下载：最多 3 条 + 头部总数/总速度 + 溢出提示", async () => {
  const harness = await boot({ activity: emptyActivity() });
  assert.equal(harness.els["active-section"].hidden, true, "无任务时整段隐藏");

  harness.activity = emptyActivity({
    active: 5,
    globalSpeedBps: 10 * MiB,
    downloads: [
      download({ id: "1", fileName: "a.iso", state: "downloading", downloaded: 50, total: 100, speedBps: MiB }),
      download({ id: "2", fileName: "b.mp4", state: "paused", downloaded: 15, total: 100 }),
      download({ id: "3", fileName: "c.zip", state: "downloading", downloaded: 62, total: 100 }),
      download({ id: "4", fileName: "d.bin", state: "downloading", downloaded: 10, total: 100 }),
      download({ id: "5", fileName: "e.bin", state: "downloading", downloaded: 10, total: 100 }),
    ],
  });
  harness.fireIntervals(POLL_MS);
  await harness.settle();

  assert.equal(harness.els["active-section"].hidden, false);
  assert.equal(harness.els["active-title"].textContent, "正在下载 5");
  assert.match(harness.els["active-speed"].textContent, /总速度 10 MiB\/s/);
  const rows = activeRows(harness);
  assert.equal(rows.length, 3, "最多渲染 3 行");
  assert.equal(rows[0].hidden, false);
  assert.equal(rows[0].className, "active-item state-downloading");
  assert.equal(rows[1].className, "active-item state-paused");
  assert.equal(rows[2].children[0].children[1].textContent, "下载中");
  assert.equal(rows[0].children[1].children[0].style.width, "50%");
  assert.match(rows[0].children[2].textContent, /50%/);
  assert.match(rows[0].children[2].textContent, /1\.0 MiB\/s/);
  assert.equal(harness.els["active-more"].hidden, false);
  assert.equal(harness.els["active-more"].textContent, "还有 2 个 · 打开 RDM");

  // 回落到 1 条：多余行隐藏、溢出消失。
  harness.activity = emptyActivity({
    downloads: [download({ id: "1", fileName: "a.iso", downloaded: 80, total: 100 })],
  });
  harness.fireIntervals(POLL_MS);
  await harness.settle();
  assert.equal(harness.els["active-title"].textContent, "正在下载 1");
  assert.equal(harness.els["active-more"].hidden, true);
  assert.equal(activeRows(harness)[1].hidden, true);
  assert.equal(activeRows(harness)[2].hidden, true);
});

test("正在下载：进度读取失败显示可操作提示（版本过旧）", async () => {
  const harness = await boot();
  harness.activity = {
    ok: false,
    guiRunning: false,
    active: 0,
    globalSpeedBps: 0,
    downloads: [],
    code: "host_outdated",
    error: "invalid_request",
  };
  harness.fireIntervals(POLL_MS);
  await harness.settle();
  assert.equal(harness.els["active-section"].hidden, false);
  assert.equal(harness.els["active-list"].hidden, true);
  assert.match(harness.els["active-hint"].textContent, /版本过旧/);
  assert.equal(harness.els["active-hint"].dataset.code, "host_outdated");
});

// ---------------------------------------------------------------------------
// 5) 最近活动 + 清空撤销
// ---------------------------------------------------------------------------

function stateWithRecent() {
  const now = Date.now();
  return baseState({
    recent: [
      recentEntry({ at: now - 60000, fileName: "1.iso", status: "handed" }),
      recentEntry({ at: now - 50000, fileName: "2.iso", status: "queued" }),
      recentEntry({ at: now - 40000, fileName: "3.iso", status: "ai" }),
      recentEntry({ at: now - 30000, fileName: "4.iso", status: "duplicate" }),
      recentEntry({ at: now - 20000, fileName: "5.iso", status: "fallback" }),
      recentEntry({ at: now - 10000, fileName: "6.iso", status: "error" }),
      recentEntry({ at: now - 5000, fileName: "7.iso", status: "handed" }),
    ],
  });
}

test("最近活动：最多 5 条、状态词符合契约、空态提示", async () => {
  const harness = await boot({ state: stateWithRecent() });
  const names = recentNames(harness);
  assert.deepEqual(names, ["7.iso", "6.iso", "5.iso", "4.iso", "3.iso"]);
  const statuses = harness.els["recent-list"].children.map(
    (row) => row.children[2].textContent
  );
  assert.deepEqual(statuses, ["已转交", "失败", "失败", "已在 RDM 中", "已解析"]);
  assert.equal(harness.els["recent-empty"].hidden, true);

  const empty = await boot({ state: baseState({ recent: [] }) });
  assert.equal(empty.els["recent-empty"].hidden, false);
  assert.equal(empty.els["recent-list"].children.length, 0);
  assert.equal(empty.els["recent-clear"].disabled, true);
});

test("最近活动清空 → toast 撤销 → 恢复列表", async () => {
  const harness = await boot({ state: stateWithRecent() });
  harness.click("recent-clear");
  await harness.settle();
  assert.ok(
    harness.sentMessages.some((message) => message.type === "clearRecent"),
    "必须发送 clearRecent"
  );
  assert.equal(harness.els["recent-list"].children.length, 0);
  assert.equal(harness.els["recent-empty"].hidden, false);
  assert.equal(harness.els["undo-toast"].hidden, false);

  harness.click("undo-action");
  await harness.settle();
  assert.ok(
    harness.sentMessages.some((message) => message.type === "restoreRecent"),
    "撤销必须发送 restoreRecent"
  );
  assert.equal(harness.els["recent-list"].children.length, 5, "撤销后恢复 5 条展示");
  assert.equal(harness.els["undo-toast"].hidden, true);

  // 5s 后 toast 自动消失（独立快照，避免被撤销逻辑干扰）。
  const autoHide = await boot({ state: stateWithRecent() });
  autoHide.click("recent-clear");
  await autoHide.settle();
  assert.equal(autoHide.els["undo-toast"].hidden, false);
  autoHide.fireTimeouts(UNDO_TTL_MS);
  assert.equal(autoHide.els["undo-toast"].hidden, true);
});

// ---------------------------------------------------------------------------
// 6) 开关 / 防重复 / openRdm / 清理
// ---------------------------------------------------------------------------

test("接管开关：乐观更新失败后回滚并提示", async () => {
  const harness = await boot({
    state: baseState(),
    setSettings: () => Promise.resolve({ ok: false, error: "storage unavailable" }),
  });
  assert.equal(harness.els["enabled"].checked, true);
  harness.els["enabled"].checked = false;
  harness.change("enabled");
  await harness.settle();
  assert.equal(harness.els["enabled"].checked, true, "失败后必须回滚");
  assert.equal(harness.els["primary-result"].textContent, "开关未保存，请重试");
  assert.equal(harness.els["primary-result"].className, "primary-result danger");
});

test("主按钮防重复：loading 期间连点只发一次请求", async () => {
  let resolveHandoff = null;
  const harness = await boot({
    tabContext: tabContext({ url: "https://example.com/a.iso" }),
    handoff: () =>
      new Promise((resolve) => {
        resolveHandoff = resolve;
      }),
  });
  harness.click("primary");
  assert.equal(harness.els["primary"].disabled, true, "提交中按钮必须禁用");
  assert.equal(harness.els["primary-text"].textContent, "正在转交…");
  harness.click("primary");
  harness.click("primary");
  await harness.settle(); // 让 sendMessage → handoff 处理器真正被调用
  const handoffs = harness.sentMessages.filter((message) => message.type === "handoff");
  assert.equal(handoffs.length, 1, "连点 3 次只能发送 1 次 handoff");
  resolveHandoff({ ok: true, message: "已交给 RDM" });
  await harness.settle();
  assert.equal(harness.els["primary"].disabled, false);
});

test("打开 RDM：宿主不支持时给出人话提示", async () => {
  const harness = await boot();
  harness.els["active-section"].hidden = false;
  harness.click("active-open");
  await harness.settle();
  assert.ok(harness.sentMessages.some((message) => message.type === "openRdm"));
  assert.match(harness.els["primary-result"].textContent, /桌面端任务栏打开/);
});

test("unload/pagehide：清理轮询与定时器，迟到触发不再请求", async () => {
  const harness = await boot();
  assert.ok(harness.intervals.size >= 2, "应有活动/相对时间两个轮询");
  const before = harness.sentMessages.filter((message) => message.type === "getActivity").length;
  harness.lifecycle("unload");
  assert.equal(harness.intervals.size, 0);
  harness.fireStaleIntervals();
  await harness.settle();
  const after = harness.sentMessages.filter((message) => message.type === "getActivity").length;
  assert.equal(after, before, "unload 后不得再轮询");

  const pagehide = await boot();
  pagehide.lifecycle("pagehide");
  assert.equal(pagehide.intervals.size, 0);
});

test("最近活动条目点击会尝试打开 RDM（不崩溃）", async () => {
  const harness = await boot({ state: stateWithRecent() });
  const row = harness.els["recent-list"].children[0];
  row.dispatch("click");
  await harness.settle();
  assert.ok(harness.sentMessages.some((message) => message.type === "openRdm"));
});

test("成功后立即触发一次活动刷新（getActivity）", async () => {
  const harness = await boot({
    tabContext: tabContext({ url: "https://example.com/a.iso" }),
  });
  const before = harness.sentMessages.filter((message) => message.type === "getActivity").length;
  harness.click("primary");
  await harness.settle();
  const after = harness.sentMessages.filter((message) => message.type === "getActivity").length;
  assert.ok(after > before, "转交成功后应刷新活动下载");
});

test("正在下载：宿主未安装时活动区不重复报错（状态条已说明）", async () => {
  const harness = await boot({
    state: baseState({ status: { host: "missing", gui: false } }),
    activity: {
      ok: false,
      guiRunning: false,
      active: 0,
      globalSpeedBps: 0,
      downloads: [],
      code: "host_unavailable",
      error: "native messaging host not found",
    },
  });
  assert.equal(harness.els["active-section"].hidden, true);
  assert.equal(harness.els["banner"].hidden, false);
  assert.match(harness.els["banner-text"].textContent, /浏览器接管尚未安装/);
});
