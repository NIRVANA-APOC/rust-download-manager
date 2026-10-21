/*!
 * popup UI 回归测试（node:test + node:vm + 手写最小 DOM/browser stub）。
 *
 * 背景：本机浏览器（Chrome/Edge headless）在 DSH 文件沙箱下会因 crashpad/mojo
 * 命名管道被拒而无法启动，真实视觉验收做不了。popup 的关键 UI 行为
 * （「正在下载」进度区/线程数/不可用提示、AI 卡紧凑结构、增量轮询/清理）
 * 的唯一自动化证据就是本文件：
 *
 *   - 用 node:vm 在独立 context 里加载真实 `lib/pure.js` + `popup.js`，
 *     不复制业务逻辑，测的就是扩展实际执行的脚本；
 *   - DOM / browser / 定时器全部手写 stub，不引入 npm 依赖；
 *   - popup.html 的元素 id 从真实文件用正则提取，避免测试与 HTML 漂移；
 *   - 伪定时器：setInterval 记录回调与 id，测试手动触发；clearInterval 记录清理。
 *
 * 运行（受限沙箱里 `node --test <dir>` 会因 test runner spawn 子进程被拒 EPERM，
 * 用 --test-isolation=none + 显式文件等价运行）：
 *   node --test --test-isolation=none \
 *     extension/firefox/test/pure.test.js \
 *     extension/firefox/test/background.test.js \
 *     extension/firefox/test/popup.test.js
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

// 宿主侧引用一次：读取 popup.js 依赖的纯函数；也用于断言未知 state 的降级。
const pure = require("../lib/pure.js");

const ROOT = path.resolve(__dirname, "..");
const POPUP_HTML = fs.readFileSync(path.join(ROOT, "popup.html"), "utf8");
const PURE_JS = fs.readFileSync(path.join(ROOT, "lib", "pure.js"), "utf8");
const POPUP_JS = fs.readFileSync(path.join(ROOT, "popup.js"), "utf8");

/** popup.html 里真实存在的元素 id（popup.js 用 getElementById 找这些节点）。 */
const HTML_IDS = [...POPUP_HTML.matchAll(/id="([^"]+)"/g)].map((match) => match[1]);

/** popup.js 必须能取到的 id；HTML 漂移时这里先失败，给出清晰原因。 */
const REQUIRED_IDS = [
  "enabled",
  "version",
  "host-dot",
  "host-text",
  "gui-dot",
  "gui-text",
  "detail",
  "refresh",
  "options",
  "clear",
  "recent-list",
  "host-name",
  "ai-parse",
  "ai-credentials",
  "ai-result",
  "active-section",
  "active-head",
  "active-title",
  "active-summary",
  "active-list",
  "active-more",
  "active-hint",
];

const MiB = 1024 * 1024;
const STATUS_POLL_MS = 1000;

// ---------------------------------------------------------------------------
// 最小 DOM stub（只实现 popup.js / 本测试用到的能力）
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
    // 与真实 DOM 一致：给容器赋 textContent 会清空子节点（renderRecent 依赖）。
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
    for (const fn of this._listeners[type] || []) fn();
  }
  querySelectorAll(selector) {
    // 只支持 refreshAges() 用到的 [data-at] 选择器。
    const out = [];
    const walk = (node) => {
      for (const child of node.children || []) {
        if (selector === "[data-at]" && child.dataset.at !== undefined) out.push(child);
        walk(child);
      }
    };
    walk(this);
    return out;
  }
}

// ---------------------------------------------------------------------------
// 测试数据与 harness
// ---------------------------------------------------------------------------

function recentEntry(overrides) {
  return Object.assign(
    {
      at: Date.now(),
      fileName: "a.iso",
      url: "https://example.com/a.iso",
      status: "handed",
      detail: "已交给 RDM（dl_a）",
      downloadId: "dl_a",
    },
    overrides || {}
  );
}

/** 3 条最近转交记录（2 handed + 1 queued）；它们不应再出现每行进度条。 */
function stateWithRecent() {
  return {
    ok: true,
    settings: { enabled: true, aiIncludeCredentials: false },
    status: { host: "ok", gui: true, detail: "" },
    recent: [
      recentEntry({ fileName: "a.iso", downloadId: "dl_a" }),
      recentEntry({
        fileName: "b.iso",
        url: "https://example.com/b.iso",
        downloadId: "dl_b",
        status: "handed",
      }),
      recentEntry({
        fileName: "c.iso",
        url: "https://example.com/c.iso",
        status: "queued",
        downloadId: "",
        detail: "RDM GUI 未运行，已排队",
      }),
    ],
    extensionVersion: "0.1.0",
    nativeHost: "com.rdm.download",
  };
}

function emptyStatus() {
  return { ok: true, guiRunning: true, active: 0, globalSpeedBps: 0, downloads: [] };
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

function statusWith(downloads, overrides) {
  const list = downloads || [];
  return Object.assign(
    {
      ok: true,
      guiRunning: true,
      active: list.length,
      globalSpeedBps: 0,
      downloads: list,
    },
    overrides || {}
  );
}

/**
 * 在独立 vm context 里加载真实 pure.js + popup.js。
 * 返回 harness：元素表、监听器表、伪定时器、浏览器调用记录与驱动方法。
 */
function createHarness(options) {
  const opts = options && typeof options === "object" ? options : {};
  const els = {};
  for (const id of HTML_IDS) els[id] = new El("div");

  const listeners = Object.create(null);
  const intervals = new Map();
  const intervalHistory = [];
  const clearedTimerIds = [];
  let nextTimerId = 1;

  const harness = {
    els,
    listeners,
    intervals,
    intervalHistory,
    clearedTimerIds,
    sentMessages: [],
    statusCalls: 0,
    // 测试可在运行中替换这些响应。
    state: opts.state || stateWithRecent(),
    status: opts.status || emptyStatus(),
    aiParseHandler: null,
  };

  const document = {
    getElementById: (id) => els[id] || null,
    createElement: (tag) => new El(tag),
  };

  const window = {
    addEventListener: (type, fn) => {
      (listeners[type] || (listeners[type] = [])).push(fn);
    },
  };

  const browser = {
    runtime: {
      sendMessage: (message) => {
        harness.sentMessages.push(message);
        if (!message || typeof message !== "object") return Promise.resolve({ ok: false });
        if (message.type === "status") {
          harness.statusCalls += 1;
          return Promise.resolve(harness.status);
        }
        if (message.type === "aiParse") {
          if (harness.aiParseHandler) return harness.aiParseHandler(message);
          return Promise.resolve({ ok: true, message: "已发送到 RDM" });
        }
        return Promise.resolve(harness.state);
      },
      openOptionsPage: () => Promise.resolve(),
    },
  };

  const sandbox = {
    document,
    window,
    browser,
    console,
    // pure.js 的 URL/TextEncoder 只在非 popup 路径用到，但提供后更接近浏览器环境。
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
      clearedTimerIds.push(id);
      intervals.delete(id);
    },
  };

  const context = vm.createContext(sandbox);
  vm.runInContext(PURE_JS, context, { filename: path.join(ROOT, "lib", "pure.js") });
  vm.runInContext(POPUP_JS, context, { filename: path.join(ROOT, "popup.js") });

  /** 手动触发所有 ms 匹配的伪 interval（popup 的 age/status 都是 1000ms）。 */
  harness.fireIntervals = (ms) => {
    for (const entry of Array.from(intervals.values())) {
      if (ms === undefined || entry.ms === ms) entry.fn();
    }
  };

  /** 模拟"已 clearInterval 的定时器迟到触发"（清理后不应再有副作用）。 */
  harness.fireStaleIntervals = () => {
    for (const entry of intervalHistory) entry.fn();
  };

  /** 排空 vm context 里的 Promise 链（真实 setTimeout 让出事件循环）。 */
  harness.settle = async () => {
    for (let i = 0; i < 10; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  };

  harness.click = (id) => {
    const element = els[id];
    assert.ok(element, "缺少元素 #" + id);
    element.dispatch("click");
  };

  return harness;
}

async function bootHarness(options) {
  const harness = createHarness(options);
  await harness.settle(); // 等 init(): loadState + detect + 立即 status 轮询
  return harness;
}

/** 最近转交的第 index 项（top/meta/[detail]）。 */
function recentItem(harness, index) {
  const item = harness.els["recent-list"].children[index];
  assert.ok(item, "最近列表缺少第 " + index + " 项");
  return item;
}

/** 「正在下载」区节点集合。 */
function activeEls(harness) {
  return {
    section: harness.els["active-section"],
    head: harness.els["active-head"],
    title: harness.els["active-title"],
    summary: harness.els["active-summary"],
    list: harness.els["active-list"],
    more: harness.els["active-more"],
    hint: harness.els["active-hint"],
  };
}

/** 「正在下载」第 index 行节点。 */
function activeRow(harness, index) {
  const row = harness.els["active-list"].children[index];
  assert.ok(row, "缺少第 " + index + " 条 active 行");
  const top = row.children[0];
  return {
    row,
    name: top.children[0],
    badge: top.children[1],
    bar: row.children[1],
    fill: row.children[1].children[0],
    text: row.children[2],
  };
}

// ---------------------------------------------------------------------------
// 1) popup.html 结构：AI 卡紧凑 + 「正在下载」区位于状态卡与 AI 卡之间
// ---------------------------------------------------------------------------

test("popup.html：AI 卡紧凑，且「正在下载」区位于状态卡之后、AI 卡之前", () => {
  for (const id of REQUIRED_IDS) {
    assert.ok(HTML_IDS.includes(id), "popup.html 缺少 popup.js 需要的 id: #" + id);
  }

  // AI 卡保持紧凑：标题/凭证/小按钮同属 .ai-head，无旧的两行长说明。
  const head = /<div class="ai-head">([\s\S]*?)<\/div>/.exec(POPUP_HTML);
  assert.ok(head, "popup.html 必须存在 .ai-head 容器");
  const block = head[1];
  for (const id of ["ai-title", "ai-credentials", "ai-parse"]) {
    assert.ok(block.includes('id="' + id + '"'), "#" + id + " 必须位于 .ai-head 同一行容器内");
  }
  assert.equal(POPUP_HTML.includes("ai-hint"), false, "旧的 .ai-hint 长说明必须移除");
  assert.match(block, /title="[^"]*Cookie[^"]*"/, "Cookie 长说明应放进 title tooltip");
  assert.match(block, /id="ai-parse"[^>]*btn-sm/, "「解析此页」应是 btn-sm 小按钮");
  assert.match(
    POPUP_HTML,
    /id="ai-result"[^>]*role="status"[^>]*aria-live="polite"/,
    "结果行必须保留 role=status + aria-live"
  );

  // 新进度区的位置：status card < active-section < ai-card。
  const statusCardIndex = POPUP_HTML.indexOf('<section class="card">');
  const activeIndex = POPUP_HTML.indexOf('id="active-section"');
  const aiCardIndex = POPUP_HTML.indexOf('class="card ai-card"');
  assert.ok(statusCardIndex >= 0 && activeIndex > statusCardIndex, "「正在下载」必须在状态卡之后");
  assert.ok(aiCardIndex > activeIndex, "「正在下载」必须在 AI 卡之前");

  // 进度区基本结构。
  assert.match(POPUP_HTML, /id="active-title">正在下载</);
  assert.match(
    POPUP_HTML,
    /id="active-hint"[^>]*role="status"[^>]*aria-live="polite"/,
    "不可用提示应是 aria-live 状态行"
  );

  // 加载顺序：纯函数模块先于 popup.js。
  assert.ok(
    POPUP_HTML.indexOf("lib/pure.js") < POPUP_HTML.indexOf("popup.js"),
    "lib/pure.js 必须先于 popup.js 加载"
  );
});

// ---------------------------------------------------------------------------
// 2) 初始渲染：最近转交不再有进度条；「正在下载」默认隐藏
// ---------------------------------------------------------------------------

test("初始渲染：最近转交 3 项无进度条，「正在下载」区默认隐藏", async () => {
  const harness = await bootHarness({ state: stateWithRecent(), status: emptyStatus() });
  const list = harness.els["recent-list"];
  assert.equal(list.children.length, 3, "2 条 handed + 1 条 queued 应渲染 3 项");
  for (let index = 0; index < 3; index += 1) {
    const item = recentItem(harness, index);
    assert.equal(item.children.length, 3, "最近转交项只应有 top/meta/detail（无进度条）");
    assert.equal(item.classList.contains("item-progress"), false);
    assert.equal(
      item.children.some((child) => (child.className || "").includes("progress")),
      false,
      "最近转交行不得再渲染进度条（避免与「正在下载」重复）"
    );
  }
  assert.equal(harness.els["recent-list"].children[0].children[0].children[1].className, "badge handed");

  const active = activeEls(harness);
  assert.equal(active.section.hidden, true, "无 active 下载时整段隐藏");
  assert.equal(active.head.hidden, true);
  assert.equal(active.list.hidden, true);
  assert.equal(active.hint.hidden, true);

  // 设置同步：开关与 Cookie 复选框来自 getState.settings。
  assert.equal(harness.els["enabled"].checked, true);
  assert.equal(harness.els["ai-credentials"].checked, false);
});

// ---------------------------------------------------------------------------
// 3) 轮询渲染：downloading（含线程数/速度）与 paused
// ---------------------------------------------------------------------------

test("轮询渲染：下载中 50% + 线程 4/8，paused 行「已暂停 · 线程 0/8」", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([
    download({
      id: "dl_a",
      fileName: "a.iso",
      state: "downloading",
      downloaded: 50,
      total: 100,
      speedBps: 20,
      connections: 8,
      activeConnections: 4,
      targetConnections: 8,
    }),
    download({
      id: "dl_b",
      fileName: "b.iso",
      state: "paused",
      downloaded: 30,
      total: 100,
      speedBps: 0,
      connections: 8,
      activeConnections: 0,
      targetConnections: 8,
    }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const active = activeEls(harness);
  assert.equal(active.section.hidden, false);
  assert.equal(active.head.hidden, false);
  assert.equal(active.title.textContent, "正在下载 2");
  assert.equal(active.summary.textContent, "总速度 20 B/s");
  assert.equal(active.more.hidden, true);
  assert.equal(active.hint.hidden, true);

  const first = activeRow(harness, 0);
  assert.equal(first.row.hidden, false);
  assert.equal(first.row.classList.contains("state-downloading"), true);
  assert.equal(first.name.textContent, "a.iso");
  assert.equal(first.badge.textContent, "下载中");
  assert.equal(first.fill.style.width, "50%");
  assert.equal(first.bar.getAttribute("aria-valuenow"), "50");
  assert.equal(first.bar.classList.contains("indeterminate"), false);
  assert.equal(first.text.textContent, "50% · 50 B / 100 B · 20 B/s · 线程 4/8");

  const second = activeRow(harness, 1);
  assert.equal(second.row.classList.contains("state-paused"), true);
  assert.equal(second.badge.textContent, "已暂停");
  assert.equal(second.bar.className, "progress state-paused");
  assert.equal(second.text.textContent, "已暂停 · 线程 0/8");

  // 最近转交不受影响（仍 3 项、无进度条）。
  assert.equal(harness.els["recent-list"].children.length, 3);
});

// ---------------------------------------------------------------------------
// 4) total=null：不确定态
// ---------------------------------------------------------------------------

test("total=null：进度条 indeterminate、无 aria-valuenow、文案不含百分比", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([
    download({
      id: "dl_a",
      state: "downloading",
      downloaded: 30 * MiB,
      total: null,
      speedBps: 10 * MiB,
      activeConnections: 2,
      targetConnections: 8,
    }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const row = activeRow(harness, 0);
  assert.equal(row.bar.classList.contains("indeterminate"), true);
  assert.equal(row.bar.hasAttribute("aria-valuenow"), false);
  assert.equal(row.fill.style.width, "");
  assert.equal(row.text.textContent, "已下载 30 MiB · 10 MiB/s · 线程 2/8");
  assert.equal(row.text.textContent.includes("%"), false, "未知总量不显示百分比");
});

// ---------------------------------------------------------------------------
// 5) 无 active → 整段隐藏
// ---------------------------------------------------------------------------

test("无 active 下载时整段隐藏（即使之前有进度）", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([
    download({ id: "dl_a", state: "downloading", downloaded: 10, total: 100 }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();
  assert.equal(activeEls(harness).section.hidden, false);

  harness.status = statusWith([
    download({ id: "dl_a", state: "completed", downloaded: 100, total: 100 }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();
  const active = activeEls(harness);
  assert.equal(active.section.hidden, true, "完成的下载不计入「正在下载」");
  assert.equal(active.list.hidden, true);
  assert.equal(active.hint.hidden, true);
});

// ---------------------------------------------------------------------------
// 6) 旧宿主（host_outdated）可操作提示
// ---------------------------------------------------------------------------

test("host_outdated：显示「请重新运行 install-firefox-host.ps1 并重启浏览器」", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = {
    ok: false,
    guiRunning: false,
    downloads: [],
    active: 0,
    globalSpeedBps: 0,
    error: "invalid_request：unknown message type",
    code: "host_outdated",
  };
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const active = activeEls(harness);
  assert.equal(active.section.hidden, false, "失败时应在同一位置显示提示");
  assert.equal(active.head.hidden, true);
  assert.equal(active.list.hidden, true);
  assert.equal(active.hint.hidden, false);
  assert.match(active.hint.textContent, /版本过旧/);
  assert.match(active.hint.textContent, /scripts\/install-firefox-host\.ps1/);
  assert.match(active.hint.textContent, /重启浏览器/);
  assert.equal(active.hint.dataset.code, "host_outdated");
});

// ---------------------------------------------------------------------------
// 7) timeout / host_unavailable 提示
// ---------------------------------------------------------------------------

test("timeout / host_unavailable：显示「原生宿主无响应，请重启浏览器后重试」", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  for (const code of ["timeout", "host_unavailable"]) {
    harness.status = {
      ok: false,
      guiRunning: false,
      downloads: [],
      active: 0,
      globalSpeedBps: 0,
      error: code,
      code,
    };
    harness.fireIntervals(STATUS_POLL_MS);
    await harness.settle();
    const active = activeEls(harness);
    assert.equal(active.section.hidden, false, code + " 应显示提示");
    assert.equal(active.hint.hidden, false);
    assert.match(active.hint.textContent, /原生宿主无响应/);
    assert.match(active.hint.textContent, /重启浏览器后重试/);
    assert.equal(active.hint.dataset.code, code);
  }
});

// ---------------------------------------------------------------------------
// 8) GUI 未运行提示（ok 响应但 guiRunning=false）
// ---------------------------------------------------------------------------

test("guiRunning=false：显示「RDM GUI 未运行：下载会先排队」提示", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = {
    ok: true,
    guiRunning: false,
    active: 0,
    globalSpeedBps: 0,
    downloads: [],
  };
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const active = activeEls(harness);
  assert.equal(active.section.hidden, false);
  assert.equal(active.head.hidden, true);
  assert.equal(active.hint.hidden, false);
  assert.match(active.hint.textContent, /RDM GUI 未运行/);
  assert.match(active.hint.textContent, /启动后这里显示进度/);
  assert.equal(active.hint.dataset.code, "gui");
});

// ---------------------------------------------------------------------------
// 9) 溢出：最多 3 条 + 「还有 N 个…」
// ---------------------------------------------------------------------------

test("超过 3 条 active：只渲染 3 行并显示「还有 2 个…」", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith(
    [1, 2, 3, 4, 5].map((index) =>
      download({
        id: "dl_" + index,
        fileName: index + ".iso",
        state: index % 2 === 0 ? "paused" : "downloading",
        downloaded: 10,
        total: 100,
      })
    )
  );
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const active = activeEls(harness);
  assert.equal(active.title.textContent, "正在下载 5");
  assert.equal(active.list.children.length, 3, "最多展示 3 行");
  assert.equal(active.list.children[0].hidden, false);
  assert.equal(active.more.hidden, false);
  assert.equal(active.more.textContent, "还有 2 个…");

  // 回落到 1 条：多余行隐藏、溢出提示消失（节点仍在，供增量复用）。
  harness.status = statusWith([download({ id: "dl_1", state: "downloading" })]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();
  assert.equal(active.more.hidden, true);
  assert.equal(active.more.textContent, "");
  assert.equal(active.title.textContent, "正在下载 1");
  assert.equal(active.list.children[1].hidden, true);
  assert.equal(active.list.children[2].hidden, true);
});

// ---------------------------------------------------------------------------
// 10) 增量更新：只改 DOM，不重建「正在下载」/「最近转交」
// ---------------------------------------------------------------------------

test("连续轮询增量更新：active 行与最近转交的节点引用都不变", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([
    download({ id: "dl_a", state: "downloading", downloaded: 10, total: 100 }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const activeList = harness.els["active-list"];
  const activeNodes = Array.from(activeList.children);
  const recentNodes = Array.from(harness.els["recent-list"].children);
  const firstRow = activeRow(harness, 0);
  const textBefore = firstRow.text.textContent;
  assert.equal(firstRow.fill.style.width, "10%");

  harness.status = statusWith([
    download({ id: "dl_a", state: "downloading", downloaded: 80, total: 100 }),
  ]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  assert.equal(activeList.children.length, activeNodes.length, "轮询不应增删 active 行");
  for (let index = 0; index < activeNodes.length; index += 1) {
    assert.equal(activeList.children[index], activeNodes[index], "active 行节点应复用");
  }
  for (let index = 0; index < recentNodes.length; index += 1) {
    assert.equal(
      harness.els["recent-list"].children[index],
      recentNodes[index],
      "轮询不应重建最近转交列表"
    );
  }
  assert.equal(activeRow(harness, 0).text, firstRow.text);
  assert.notEqual(firstRow.text.textContent, textBefore);
  assert.equal(firstRow.fill.style.width, "80%");
});

// ---------------------------------------------------------------------------
// 11) unload / pagehide 清理
// ---------------------------------------------------------------------------

test("unload/pagehide：清理 setInterval，之后迟到触发也不再发 status", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([download({ id: "dl_a", state: "downloading" })]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();
  assert.equal(activeEls(harness).section.hidden, false);

  assert.ok(harness.intervals.size >= 2, "应有 age/status 两个轮询定时器");
  const statusBefore = harness.statusCalls;

  for (const fn of harness.listeners.unload || []) fn();
  assert.equal(harness.intervals.size, 0, "unload 后不应有活动 interval");
  assert.ok(harness.clearedTimerIds.length >= 2, "clearInterval 必须清理所有定时器");

  // 模拟已清理的定时器迟到触发：closed 守卫应阻止再次发 status。
  harness.fireStaleIntervals();
  await harness.settle();
  assert.equal(harness.statusCalls, statusBefore, "unload 后不得再轮询 status");

  const pagehide = await bootHarness({ state: stateWithRecent() });
  for (const fn of pagehide.listeners.pagehide || []) fn();
  assert.equal(pagehide.intervals.size, 0, "pagehide 后不应有活动 interval");
  assert.ok(pagehide.clearedTimerIds.length >= 2);
});

// ---------------------------------------------------------------------------
// 12) AI 解析按钮流程（保持紧凑卡不变）
// ---------------------------------------------------------------------------

test("AI 按钮：执行中禁用/文案，成功 ok、失败 bad，结束后恢复", async () => {
  const harness = await bootHarness({ state: stateWithRecent() });
  assert.equal(harness.els["ai-credentials"].checked, false);

  let resolveAiParse = null;
  harness.aiParseHandler = () =>
    new Promise((resolve) => {
      resolveAiParse = resolve;
    });

  harness.click("ai-parse");
  // 等待 sendMessage 前同步设置的执行中状态。
  assert.equal(harness.els["ai-parse"].disabled, true, "执行期间按钮应禁用");
  assert.equal(harness.els["ai-parse"].textContent, "解析中…");
  assert.equal(harness.els["ai-result"].className, "ai-result busy");

  const sent = harness.sentMessages.filter((message) => message.type === "aiParse").pop();
  assert.ok(sent, "点击后必须发送 aiParse");
  assert.equal(sent.type, "aiParse");
  assert.equal(sent.includeCredentials, false, "未勾选 Cookie 时 includeCredentials=false");

  resolveAiParse({ ok: true, message: "已发送到 RDM（#12），解析结果在桌面端 AI 栏查看" });
  await harness.settle();
  assert.equal(harness.els["ai-parse"].disabled, false);
  assert.equal(harness.els["ai-parse"].textContent, "解析此页");
  assert.equal(harness.els["ai-result"].className, "ai-result ok");
  assert.match(harness.els["ai-result"].textContent, /已发送到 RDM/);

  // 失败路径：class=bad，按钮仍恢复。
  harness.aiParseHandler = () =>
    Promise.resolve({ ok: false, message: "AI 解析未发送：RDM 未运行" });
  harness.click("ai-parse");
  await harness.settle();
  assert.equal(harness.els["ai-result"].className, "ai-result bad");
  assert.match(harness.els["ai-result"].textContent, /未发送/);
  assert.equal(harness.els["ai-parse"].disabled, false);
  assert.equal(harness.els["ai-parse"].textContent, "解析此页");
});

// ---------------------------------------------------------------------------
// 13) 未知 state（契约外）→ 静默隐藏，不显示错误进度条
// ---------------------------------------------------------------------------

test("未知 state（契约外）：normalizeDownload 丢弃，进度区静默隐藏", async () => {
  assert.equal(pure.normalizeDownload({ id: "dl_a", state: "unknown" }), null);

  const harness = await bootHarness({ state: stateWithRecent() });
  harness.status = statusWith([download({ id: "dl_a", state: "unknown" })]);
  harness.fireIntervals(STATUS_POLL_MS);
  await harness.settle();

  const active = activeEls(harness);
  assert.equal(active.section.hidden, true);
  assert.equal(active.hint.hidden, true);
  assert.equal(active.list.children.length, 0, "不应为坏数据创建进度行");
});
