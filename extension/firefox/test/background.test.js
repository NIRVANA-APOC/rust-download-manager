/*!
 * background.js 的装配冒烟测试（Node + 手写 browser API stub）。
 *
 * 目的不是测业务细节（纯逻辑在 pure.test.js），而是确保：
 *   - 后台脚本能在"没有真实浏览器"的环境下完整执行，不抛 ReferenceError；
 *   - 所有必须的事件监听器都在顶层同步注册；
 *   - runtime.onMessage 的 getState / testConnection 能返回结构化快照；
 *   - 原生宿主缺失时能归类为 host="missing" 而不是崩掉。
 *
 * 运行：node --test extension/firefox/test/
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

function createEvent() {
  const listeners = [];
  return {
    listeners,
    addListener: (fn) => listeners.push(fn),
    removeListener: (fn) => {
      const index = listeners.indexOf(fn);
      if (index >= 0) listeners.splice(index, 1);
    },
    hasListener: (fn) => listeners.includes(fn),
  };
}

function createPortStub(mode) {
  const messageListeners = [];
  const disconnectListeners = [];
  const port = {
    error: null,
    posted: [],
    onMessage: {
      addListener: (fn) => messageListeners.push(fn),
    },
    onDisconnect: {
      addListener: (fn) => disconnectListeners.push(fn),
    },
    postMessage: (message) => {
      port.posted.push(message);
      setTimeout(() => {
        if (mode === "missing") {
          port.error = new Error("No such native application com.rdm.download");
          for (const fn of disconnectListeners) fn(port);
          return;
        }
        for (const fn of messageListeners) {
          if (message.type === "ping") {
            fn({ type: "pong", id: message.id, version: "0.1.0", gui_running: true, protocol: 1 });
          } else if (message.type === "enqueue") {
            if (mode === "queued") {
              fn({
                type: "queued",
                id: message.id,
                reason: "gui_unavailable",
                spool: "C:\\Temp\\rdm\\queue\\1-1-0.json",
                credentials_stripped: true,
              });
            } else {
              fn({
                type: "enqueued",
                id: message.id,
                download_id: "dl_000042",
                gui_running: true,
              });
            }
          }
        }
      }, 0);
    },
    disconnect: () => {},
    emit: (message) => {
      for (const fn of messageListeners) fn(message);
    },
  };
  return port;
}

function createBrowserStub(options) {
  const settings = options && options.settings ? options.settings : {};
  const storage = { ...settings };
  const events = {
    onMessage: createEvent(),
    onInstalled: createEvent(),
    onStartup: createEvent(),
    onBeforeRequest: createEvent(),
    onBeforeSendHeaders: createEvent(),
    onHeadersReceived: createEvent(),
    onBeforeRedirect: createEvent(),
    onCreated: createEvent(),
    onChanged: createEvent(),
    onClicked: createEvent(),
    onAlarm: createEvent(),
  };
  const menus = [];
  const calls = { downloadsDownload: [], storageSet: [] };

  const browser = {
    runtime: {
      id: "rdm@rdm.download",
      lastError: null,
      getManifest: () => ({ version: "0.1.0", manifest_version: 3 }),
      onMessage: events.onMessage,
      onInstalled: events.onInstalled,
      onStartup: events.onStartup,
      connectNative: () => createPortStub(options && options.nativeMode ? options.nativeMode : "ok"),
      openOptionsPage: () => Promise.resolve(),
      sendMessage: () => Promise.resolve({ ok: true }),
    },
    storage: {
      local: {
        get: (key) => Promise.resolve({ [key]: storage[key] }),
        set: (values) => {
          calls.storageSet.push(values);
          Object.assign(storage, values);
          return Promise.resolve();
        },
      },
      onChanged: { addListener: () => {} },
    },
    webRequest: {
      onBeforeRequest: events.onBeforeRequest,
      onBeforeSendHeaders: events.onBeforeSendHeaders,
      onHeadersReceived: events.onHeadersReceived,
      onBeforeRedirect: events.onBeforeRedirect,
    },
    downloads: {
      onCreated: events.onCreated,
      onChanged: events.onChanged,
      cancel: () => Promise.resolve(),
      erase: () => Promise.resolve([1]),
      removeFile: () => Promise.resolve(),
      search: () => Promise.resolve([]),
      download: (options) => {
        calls.downloadsDownload.push(options);
        return Promise.resolve(42);
      },
    },
    contextMenus: {
      onClicked: events.onClicked,
      create: (props) => {
        menus.push(props);
        return Promise.resolve(props.id);
      },
      remove: () => Promise.resolve(),
    },
    alarms: {
      onAlarm: events.onAlarm,
      get: () => Promise.resolve(undefined),
      create: () => Promise.resolve(),
    },
    action: {
      setBadgeText: () => Promise.resolve(),
      setBadgeBackgroundColor: () => Promise.resolve(),
      setTitle: () => Promise.resolve(),
    },
    cookies: {
      getAll: () => Promise.resolve([{ name: "sid", value: "secret" }]),
    },
  };

  return { browser, events, menus, calls, storage };
}

async function loadBackground(options) {
  // 清掉 require 缓存，保证每个测试进程内拿到干净的后台脚本状态。
  for (const key of Object.keys(require.cache)) {
    if (key.includes("extension") && (key.endsWith("background.js") || key.endsWith("pure.js"))) {
      delete require.cache[key];
    }
  }
  const stub = createBrowserStub(options);
  globalThis.browser = stub.browser;
  require("../lib/pure.js");
  require("../background.js");
  // 让顶层 initialize() 的异步任务跑完
  await new Promise((resolve) => setTimeout(resolve, 30));
  return stub;
}

test("background.js 顶层注册了全部必需的事件监听器", async () => {
  const { events } = await loadBackground();
  assert.equal(events.onMessage.listeners.length >= 1, true, "runtime.onMessage 未注册");
  assert.equal(events.onBeforeRequest.listeners.length, 1);
  assert.equal(events.onBeforeSendHeaders.listeners.length, 1);
  assert.equal(events.onHeadersReceived.listeners.length, 1);
  assert.equal(events.onBeforeRedirect.listeners.length, 1);
  assert.equal(events.onCreated.listeners.length, 1);
  assert.equal(events.onClicked.listeners.length, 1);
  assert.equal(events.onAlarm.listeners.length, 1);
  assert.equal(events.onInstalled.listeners.length, 1);
  assert.equal(events.onStartup.listeners.length, 1);
});

test("getState 返回完整快照（设置 / 状态 / 最近记录）", async () => {
  const { events } = await loadBackground();
  const handler = events.onMessage.listeners[0];
  const state = await handler({ type: "getState" });
  assert.equal(state.ok, true);
  assert.equal(typeof state.settings, "object");
  assert.equal(state.settings.connections, 8);
  assert.equal(state.settings.autoStartGui, true);
  assert.ok(Array.isArray(state.recent));
  assert.equal(typeof state.status, "object");
  assert.equal(state.extensionVersion, "0.1.0");
  assert.equal(state.nativeHost, "com.rdm.download");
});

test("原生宿主缺失时 testConnection 归类为 missing，而不是抛异常", async () => {
  const { events } = await loadBackground({ nativeMode: "missing" });
  const handler = events.onMessage.listeners[0];
  const state = await handler({ type: "testConnection" });
  assert.equal(state.ok, true);
  assert.equal(state.status.host, "missing");
  assert.match(state.status.detail, /未安装|install/i);
});

test("setSettings 归一化并持久化，同时更新菜单", async () => {
  const { events, calls, menus } = await loadBackground();
  const handler = events.onMessage.listeners[0];
  const state = await handler({
    type: "setSettings",
    settings: {
      enabled: false,
      connections: 999,
      minSizeBytes: 1024 * 1024,
      allowExtensions: [".ISO", "iso", "bad ext"],
    },
  });
  assert.equal(state.ok, true);
  assert.equal(state.settings.connections, 32);
  assert.deepEqual(state.settings.allowExtensions, ["iso"]);
  assert.equal(state.settings.enabled, false);
  const persisted = calls.storageSet.find((values) => values["rdm.settings"]);
  assert.ok(persisted, "设置必须写入 storage");
  // setSettings 会重建右键菜单（默认 contextMenu=true，所以 create 必须被调用）。
  assert.ok(menus.length >= 1, "保存设置后应重建右键菜单");
  assert.equal(menus[0].title, "使用 RDM 下载");
});

test("未知消息类型返回结构化错误", async () => {
  const { events } = await loadBackground();
  const handler = events.onMessage.listeners[0];
  const result = await handler({ type: "definitely-not-a-message" });
  assert.deepEqual(result, { ok: false, error: "unknown_message_type" });
});

test("webRequest 观察回调不会因未知请求崩溃", async () => {
  const { events } = await loadBackground();
  const beforeRequest = events.onBeforeRequest.listeners[0];
  beforeRequest({ url: "https://example.com/big.iso", method: "GET", documentUrl: "https://example.com/" });
  beforeRequest({ url: "data:text/plain,hi" });
  const beforeSendHeaders = events.onBeforeSendHeaders.listeners[0];
  beforeSendHeaders({
    url: "https://example.com/big.iso",
    requestHeaders: [{ name: "User-Agent", value: "Mozilla/5.0 test" }],
  });
  const headersReceived = events.onHeadersReceived.listeners[0];
  headersReceived({
    url: "https://example.com/big.iso",
    responseHeaders: [
      { name: "Content-Disposition", value: 'attachment; filename="big.iso"' },
      { name: "Content-Length", value: "1048576" },
    ],
  });
});

test("downloads.onCreated 对 data:/blob: 等非 http 下载直接跳过", async () => {
  const { events, calls } = await loadBackground();
  const onCreated = events.onCreated.listeners[0];
  await onCreated({
    id: 1,
    url: "data:text/plain,hi",
    filename: "hi.txt",
    totalBytes: 2,
  });
  await onCreated({
    id: 2,
    url: "blob:https://example.com/abc",
    filename: "x.bin",
    totalBytes: -1,
  });
  assert.equal(calls.downloadsDownload.length, 0, "不应触发任何下载/回退");
});

test("queued 响应会记录「登录态/授权信息不会保存」的文案", async () => {
  const { events, storage } = await loadBackground({ nativeMode: "queued" });
  const onCreated = events.onCreated.listeners[0];
  await onCreated({
    id: 42,
    url: "https://example.com/big.iso",
    filename: "C:\\Users\\me\\Downloads\\big.iso",
    totalBytes: 8 * 1024 * 1024,
    referrer: "https://example.com/list",
  });

  // onCreated 处理是 fire-and-forget，轮询等待它写入 recent。
  const deadline = Date.now() + 3000;
  let recent = [];
  while (Date.now() < deadline) {
    recent = storage["rdm.recent"] || [];
    if (recent.length > 0) break;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.equal(recent.length, 1, "queued 后必须写入一条最近转交记录");
  assert.equal(recent[0].status, "queued");
  assert.match(recent[0].detail, /登录态\/授权信息不会保存/);
  assert.match(recent[0].detail, /先启动 RDM 后重新下载/);
});
