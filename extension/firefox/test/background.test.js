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

function createPortStub(mode, aiMode, statusMode) {
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
          } else if (message.type === "status") {
            if (statusMode === "timeout") {
              // 故意不响应：触发 background 的 3s 超时分支（code="timeout"）。
              return;
            }
            if (statusMode === "old-host") {
              // 旧版原生宿主不认识 status 的典型响应。
              fn({
                type: "error",
                id: message.id,
                code: "invalid_request",
                message: "unknown message type",
              });
            } else if (statusMode === "error") {
              fn({
                type: "error",
                id: message.id,
                code: "gui_unavailable",
                message: "GUI is not running; status unavailable",
              });
            } else if (statusMode === "bad") {
              fn({
                type: "status",
                id: message.id,
                ok: true,
                gui_running: true,
                active: "many",
                global_speed_bps: -1,
                downloads: [
                  null,
                  { id: 42, file_name: "numeric-id.bin", state: "downloading" },
                  { id: "dl_bad", file_name: 7, state: "exploded" },
                  {
                    id: "dl_unknown",
                    file_name: "unknown.bin",
                    state: "downloading",
                    downloaded: "512",
                    total: null,
                    speed_bps: "1024",
                    connections: 8,
                    active_connections: 2,
                    target_connections: "6",
                    peak_connections: -1,
                  },
                  {
                    id: "dl_done",
                    file_name: "done.bin",
                    state: "completed",
                    downloaded: 10,
                    total: 10,
                    speed_bps: 0,
                    connections: 2,
                  },
                ],
              });
            } else {
              fn({
                type: "status",
                id: message.id,
                ok: true,
                gui_running: true,
                active: 1,
                global_speed_bps: 2048.0,
                downloads: [
                  {
                    id: "dl_000042",
                    file_name: "big.iso",
                    state: "downloading",
                    downloaded: 512,
                    total: 1024,
                    speed_bps: 512.0,
                    connections: 8,
                    active_connections: 4,
                    target_connections: 8,
                    peak_connections: 16,
                  },
                  {
                    id: "dl_000043",
                    file_name: "done.bin",
                    state: "completed",
                    downloaded: 10,
                    total: 10,
                    speed_bps: 0.0,
                    connections: 4,
                  },
                ],
              });
            }
          } else if (message.type === "ai_parse") {
            if (aiMode === "error") {
              fn({
                type: "error",
                id: message.id,
                code: "gui_unavailable",
                message: "RDM GUI is not running; AI 解析需要桌面端在线（不会排队）",
              });
            } else {
              fn({
                type: "ai_parse_accepted",
                id: message.id,
                ok: true,
                request_id: 12,
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
  const ports = [];
  const calls = { downloadsDownload: [], storageSet: [], executeScript: [] };

  const browser = {
    runtime: {
      id: "rdm@rdm.download",
      lastError: null,
      getManifest: () => ({ version: "0.1.0", manifest_version: 3 }),
      onMessage: events.onMessage,
      onInstalled: events.onInstalled,
      onStartup: events.onStartup,
      connectNative: () => {
        const port = createPortStub(
          options && options.nativeMode ? options.nativeMode : "ok",
          options && options.aiMode ? options.aiMode : "ok",
          options && options.statusMode ? options.statusMode : "ok"
        );
        ports.push(port);
        return port;
      },
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
    tabs: {
      query: () =>
        Promise.resolve([
          { id: 7, url: "https://example.com/page", title: "Example Page" },
        ]),
    },
    scripting: {
      executeScript: (details) => {
        calls.executeScript.push(details);
        if (options && options.scriptingMode === "error") {
          return Promise.reject(new Error("Cannot access contents of the page"));
        }
        if (options && options.scriptingMode === "empty") {
          return Promise.resolve([]);
        }
        const snapshotUrl =
          options && options.snapshotUrl
            ? options.snapshotUrl
            : "https://example.com/page";
        const links =
          options && Array.isArray(options.snapshotLinks)
            ? options.snapshotLinks
            : ["https://cdn.example.com/a.iso", "https://cdn.example.com/b.zip"];
        return Promise.resolve([
          {
            frameId: 0,
            result: { url: snapshotUrl, title: "Example Page", links },
          },
        ]);
      },
    },
  };

  return { browser, events, menus, ports, calls, storage };
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
  // 连接数由 RDM 桌面端统一决定，扩展设置里不再有这一项。
  assert.equal("connections" in state.settings, false);
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
      minSizeBytes: 1024 * 1024,
      allowExtensions: [".ISO", "iso", "bad ext"],
    },
  });
  assert.equal(state.ok, true);
  assert.equal("connections" in state.settings, false, "设置里不得再出现 connections");
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
  const { events, storage, ports } = await loadBackground({ nativeMode: "queued" });
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

  // 防回退：enqueue.request 不得再携带扩展自己的连接数（GUI 的默认连接数是唯一权威）。
  const enqueue = await waitFor(() => {
    for (const port of ports) {
      const message = port.posted.find((entry) => entry && entry.type === "enqueue");
      if (message) return message;
    }
    return null;
  });
  assert.ok(enqueue, "queued 路径也必须发送 enqueue");
  assert.equal(
    "connections" in enqueue.request,
    false,
    "扩展不得再发送 connections，连接数由 RDM 桌面端统一决定"
  );
});

// ---------------------------------------------------------------------------
// AI 解析（ai_parse）
// ---------------------------------------------------------------------------

function findAiParse(stub) {
  for (const port of stub.ports) {
    const sent = port.posted.find((message) => message && message.type === "ai_parse");
    if (sent) return sent;
  }
  return null;
}

async function waitFor(predicate, timeoutMs) {
  const deadline = Date.now() + (timeoutMs || 3000);
  while (Date.now() < deadline) {
    const value = predicate();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  return predicate();
}

test("AI 解析：取当前标签页、按勾选附带 Cookie，返回「已发送到 RDM」", async () => {
  const stub = await loadBackground();
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse", includeCredentials: true });
  assert.equal(result.ok, true);
  assert.match(result.message, /已发送到 RDM/);
  assert.match(result.message, /AI 栏/);

  const sent = findAiParse(stub);
  assert.ok(sent, "必须通过 native port 发送 ai_parse");
  assert.equal(sent.url, "https://example.com/page");
  assert.equal(sent.title, "Example Page");
  assert.equal(sent.source_page, "https://example.com/page");
  assert.equal(sent.include_credentials, true);
  assert.equal(sent.cookie, "sid=secret");
  assert.equal(sent.launch_gui, true);
  assert.equal(typeof sent.id, "number");

  // 隐私：调试日志只允许出现脱敏 URL，绝不能出现 Cookie 值。
  const state = await handler({ type: "getState" });
  const debugText = (state.debug || []).join("\n");
  assert.match(debugText, /AI 解析请求/);
  assert.equal(debugText.includes("secret"), false, "日志不得包含 Cookie 值");
});

test("AI 解析：默认不附带 Cookie（设置默认关闭）", async () => {
  const stub = await loadBackground();
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse" });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.ok(sent);
  assert.equal(sent.include_credentials, false);
  assert.equal("cookie" in sent, false, "默认不得携带 Cookie");
});

test("AI 解析：持久化的凭证开关生效", async () => {
  const stub = await loadBackground();
  const handler = stub.events.onMessage.listeners[0];
  const settingsState = await handler({
    type: "setSettings",
    settings: { aiIncludeCredentials: true },
  });
  assert.equal(settingsState.settings.aiIncludeCredentials, true);

  const result = await handler({ type: "aiParse" });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.equal(sent.include_credentials, true);
  assert.equal(sent.cookie, "sid=secret");
});

test("AI 解析失败：gui_unavailable 返回可读文案", async () => {
  const stub = await loadBackground({ aiMode: "error" });
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse" });
  assert.equal(result.ok, false);
  assert.equal(result.code, "gui_unavailable");
  assert.match(result.message, /RDM 未运行/);
  assert.match(result.message, /不会排队/);
});

test("AI 解析：勾选凭证时采集页面链接快照并附带 page_url/page_title/page_links", async () => {
  const stub = await loadBackground();
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse", includeCredentials: true });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.ok(sent, "必须发送 ai_parse");
  assert.equal(sent.page_url, "https://example.com/page");
  assert.equal(sent.page_title, "Example Page");
  assert.deepEqual(sent.page_links, [
    "https://cdn.example.com/a.iso",
    "https://cdn.example.com/b.zip",
  ]);
  assert.equal(stub.calls.executeScript.length, 1, "凭证同意时才采集一次快照");
  assert.deepEqual(stub.calls.executeScript[0].target, {
    tabId: 7,
    allFrames: false,
  });
  assert.equal(typeof stub.calls.executeScript[0].func, "function");

  // 隐私：日志只记数量，绝不记录链接值/标题值/Cookie 值。
  const state = await handler({ type: "getState" });
  const debugText = (state.debug || []).join("\n");
  assert.match(debugText, /页面链接快照: links=2/);
  assert.equal(debugText.includes("cdn.example.com"), false, "日志不得包含链接值");
  assert.equal(debugText.includes("Example Page"), false, "日志不得包含标题值");
  assert.equal(debugText.includes("secret"), false, "日志不得包含 Cookie 值");
});

test("AI 解析：未勾选凭证时不采集页面快照（默认不采集）", async () => {
  const stub = await loadBackground();
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse" });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.ok(sent);
  assert.equal("page_url" in sent, false);
  assert.equal("page_title" in sent, false);
  assert.equal("page_links" in sent, false);
  assert.equal(stub.calls.executeScript.length, 0, "未勾选凭证不得调用 scripting");
});

test("AI 解析：scripting 失败不阻塞解析，消息不含 page 字段", async () => {
  const stub = await loadBackground({ scriptingMode: "error" });
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse", includeCredentials: true });
  assert.equal(result.ok, true, "快照失败不能阻塞解析");
  const sent = findAiParse(stub);
  assert.ok(sent, "仍必须发送 ai_parse");
  assert.equal("page_url" in sent, false);
  assert.equal("page_links" in sent, false);
  const state = await handler({ type: "getState" });
  const debugText = (state.debug || []).join("\n");
  assert.match(debugText, /采集页面链接快照失败/);
  assert.equal(debugText.includes("cdn.example.com"), false);
});

test("AI 解析：scripting 返回空结果时不附带 page 字段", async () => {
  const stub = await loadBackground({ scriptingMode: "empty" });
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse", includeCredentials: true });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.equal("page_url" in sent, false);
});

test("AI 解析：快照 URL 与目标页面不一致时忽略整组快照", async () => {
  const stub = await loadBackground({
    snapshotUrl: "https://other.example/elsewhere",
  });
  const handler = stub.events.onMessage.listeners[0];
  const result = await handler({ type: "aiParse", includeCredentials: true });
  assert.equal(result.ok, true);
  const sent = findAiParse(stub);
  assert.equal("page_url" in sent, false);
  assert.equal("page_links" in sent, false);
});

test("AI 解析：右键菜单路径带标签页 id 采集快照", async () => {
  const stub = await loadBackground({
    settings: { "rdm.settings": { aiIncludeCredentials: true } },
  });
  stub.events.onInstalled.listeners[0]();
  const aiMenu = await waitFor(() =>
    stub.menus.find((menu) => menu.title === "用 RDM AI 解析此页")
  );
  assert.ok(aiMenu);
  const onClicked = stub.events.onClicked.listeners[0];
  onClicked(
    { menuItemId: "rdm-ai-parse-page" },
    { id: 7, url: "https://example.com/page", title: "Example Page" }
  );
  const sent = await waitFor(() => findAiParse(stub));
  assert.ok(sent);
  assert.equal(sent.page_url, "https://example.com/page");
  assert.equal(stub.calls.executeScript.length, 1);
  assert.deepEqual(stub.calls.executeScript[0].target.tabId, 7);
});

test("右键菜单包含「用 RDM AI 解析此页」，点击后发送 ai_parse 并记录最近条目", async () => {
  const stub = await loadBackground();
  // onInstalled 会重建右键菜单；等它注册完成再断言。
  stub.events.onInstalled.listeners[0]();
  const aiMenu = await waitFor(() =>
    stub.menus.find((menu) => menu.title === "用 RDM AI 解析此页")
  );
  assert.ok(aiMenu, "必须创建 AI 解析右键菜单");
  assert.ok(
    stub.menus.some((menu) => menu.title === "使用 RDM 下载"),
    "原有下载右键菜单不能丢"
  );
  const onClicked = stub.events.onClicked.listeners[0];
  onClicked(
    { menuItemId: "rdm-ai-parse-page" },
    { url: "https://example.com/page", title: "Example Page" }
  );

  const recent = await waitFor(() => {
    const list = stub.storage["rdm.recent"] || [];
    return list.length > 0 ? list : null;
  });
  assert.ok(recent, "AI 解析后必须写入最近记录");
  assert.equal(recent[0].status, "ai");
  assert.equal(recent[0].fileName, "Example Page");
  assert.equal(recent[0].url, "https://example.com/page");
  assert.match(recent[0].detail, /已发送到 RDM/);
  assert.ok(findAiParse(stub), "右键菜单必须发送 ai_parse");
});

test("右键菜单「使用 RDM 下载」仍走下载转交", async () => {
  const stub = await loadBackground();
  const onClicked = stub.events.onClicked.listeners[0];
  onClicked(
    { menuItemId: "rdm-download-with", linkUrl: "https://example.com/big.iso" },
    { url: "https://example.com/page" }
  );

  const sent = await waitFor(() => {
    for (const port of stub.ports) {
      const message = port.posted.find((entry) => entry && entry.type === "enqueue");
      if (message) return message;
    }
    return null;
  });
  assert.ok(sent, "右键下载必须发送 enqueue");
  assert.equal(sent.request.url, "https://example.com/big.iso");
  assert.equal(sent.request.source_page, "https://example.com/page");
  // 防回退：转交请求不带 connections（由 RDM 桌面端「默认连接数」统一决定）。
  assert.equal("connections" in sent.request, false);
  // 其它下载参数仍照旧透传。
  assert.equal(sent.request.source, "firefox");
});

// ---------------------------------------------------------------------------
// 下载进度（status）
// ---------------------------------------------------------------------------

test("status 消息把宿主 snake_case 归一化为 popup 契约的 camelCase（含线程三字段）", async () => {
  const { events } = await loadBackground();
  const handler = events.onMessage.listeners[0];
  const status = await handler({ type: "status" });
  assert.deepEqual(Object.keys(status).sort(), [
    "active",
    "downloads",
    "globalSpeedBps",
    "guiRunning",
    "ok",
  ]);
  assert.equal(status.ok, true);
  assert.equal(status.guiRunning, true);
  assert.equal(status.active, 1);
  assert.equal(status.globalSpeedBps, 2048);
  assert.equal(status.downloads.length, 2);
  assert.deepEqual(status.downloads[0], {
    id: "dl_000042",
    fileName: "big.iso",
    state: "downloading",
    downloaded: 512,
    total: 1024,
    speedBps: 512,
    connections: 8,
    activeConnections: 4,
    targetConnections: 8,
    peakConnections: 16,
  });
  // 不允许漏出 snake_case 字段
  assert.equal("file_name" in status.downloads[0], false);
  assert.equal("speed_bps" in status.downloads[0], false);
  assert.equal("active_connections" in status.downloads[0], false);
  // 旧宿主缺省字段 → 0 兜底
  assert.deepEqual(
    [
      status.downloads[1].activeConnections,
      status.downloads[1].targetConnections,
      status.downloads[1].peakConnections,
    ],
    [0, 0, 0]
  );
  assert.equal(status.downloads[1].state, "completed");
});

test("宿主缺失时 status 返回 ok:false + code=host_unavailable（不抛错）", async () => {
  const { events } = await loadBackground({ nativeMode: "missing" });
  const handler = events.onMessage.listeners[0];
  const status = await handler({ type: "status" });
  assert.equal(status.ok, false);
  assert.equal(status.guiRunning, false);
  assert.deepEqual(status.downloads, []);
  assert.equal(status.active, 0);
  assert.equal(status.globalSpeedBps, 0);
  assert.equal(status.code, "host_unavailable");
  assert.equal(typeof status.error, "string");
  assert.match(status.error, /未安装|not found|native/i);
});

test("旧宿主不认 status（error/invalid_request）→ code=host_outdated", async () => {
  const { events } = await loadBackground({ statusMode: "old-host" });
  const handler = events.onMessage.listeners[0];
  const status = await handler({ type: "status" });
  assert.equal(status.ok, false);
  assert.deepEqual(status.downloads, []);
  assert.equal(status.code, "host_outdated");
  assert.match(status.error, /invalid_request/);
  assert.match(status.error, /unknown message type/);
});

test("status 请求超时 → code=timeout（不抛错）", async () => {
  const { events } = await loadBackground({ statusMode: "timeout" });
  const handler = events.onMessage.listeners[0];
  const startedAt = Date.now();
  const status = await handler({ type: "status" });
  const elapsed = Date.now() - startedAt;
  assert.equal(status.ok, false);
  assert.deepEqual(status.downloads, []);
  assert.equal(status.code, "timeout");
  assert.match(status.error, /超时|timeout/i);
  assert.ok(elapsed >= 2500, "应等待 ~3s 超时而不是立即失败，实际 " + elapsed + "ms");
});

test("status 宿主坏数据：非法下载项丢弃、数值字段回退，不抛错", async () => {
  const { events } = await loadBackground({ statusMode: "bad" });
  const handler = events.onMessage.listeners[0];
  const status = await handler({ type: "status" });
  assert.equal(status.ok, true);
  assert.equal(status.guiRunning, true);
  assert.equal(status.active, 0, "非数字 active 应回退 0");
  assert.equal(status.globalSpeedBps, 0, "负数速度应回退 0");
  assert.equal(status.downloads.length, 2, "null / 数字 id / 非法 state 应丢弃");
  assert.deepEqual(
    status.downloads.map((item) => item.id),
    ["dl_unknown", "dl_done"]
  );
  assert.equal(status.downloads[0].total, null);
  assert.equal(status.downloads[0].downloaded, 512, "数字字符串应可解析");
  assert.equal(status.downloads[0].speedBps, 1024);
  assert.equal(status.downloads[0].fileName, "unknown.bin");
  // 线程三字段：数字字符串可解析，负数/缺失 → 0
  assert.equal(status.downloads[0].activeConnections, 2);
  assert.equal(status.downloads[0].targetConnections, 6);
  assert.equal(status.downloads[0].peakConnections, 0);
  assert.equal(status.downloads[1].activeConnections, 0);
});

test("status 宿主结构化 error（非旧宿主）→ code=error", async () => {
  const { events } = await loadBackground({ statusMode: "error" });
  const handler = events.onMessage.listeners[0];
  const status = await handler({ type: "status" });
  assert.equal(status.ok, false);
  assert.deepEqual(status.downloads, []);
  assert.equal(status.code, "error");
  assert.match(status.error, /gui_unavailable/);
});

test("handed 最近记录写入 downloadId；queued 记录为空串", async () => {
  const handed = await loadBackground();
  const onCreated = handed.events.onCreated.listeners[0];
  await onCreated({
    id: 42,
    url: "https://example.com/big.iso",
    filename: "C:\\Users\\me\\Downloads\\big.iso",
    totalBytes: 8 * 1024 * 1024,
  });
  const handedRecent = await waitFor(() => {
    const list = handed.storage["rdm.recent"] || [];
    return list.length > 0 ? list : null;
  });
  assert.ok(handedRecent, "handed 后必须写入最近记录");
  assert.equal(handedRecent[0].status, "handed");
  assert.equal(handedRecent[0].downloadId, "dl_000042");

  const queued = await loadBackground({ nativeMode: "queued" });
  const onCreatedQueued = queued.events.onCreated.listeners[0];
  await onCreatedQueued({
    id: 43,
    url: "https://example.com/other.iso",
    filename: "other.iso",
    totalBytes: 1024,
  });
  const queuedRecent = await waitFor(() => {
    const list = queued.storage["rdm.recent"] || [];
    return list.length > 0 ? list : null;
  });
  assert.ok(queuedRecent, "queued 后必须写入最近记录");
  assert.equal(queuedRecent[0].status, "queued");
  assert.equal(queuedRecent[0].downloadId, "");
});

test("旧 storage 里没有 downloadId 的最近记录：getState 补空串而不崩", async () => {
  const { events } = await loadBackground({
    settings: {
      "rdm.recent": [
        {
          at: Date.now(),
          fileName: "old.iso",
          url: "https://example.com/old.iso",
          status: "handed",
          detail: "已交给 RDM",
        },
      ],
    },
  });
  const handler = events.onMessage.listeners[0];
  const state = await handler({ type: "getState" });
  assert.equal(state.ok, true);
  assert.equal(state.recent.length, 1);
  assert.equal(state.recent[0].fileName, "old.iso");
  assert.equal(state.recent[0].downloadId, "");
});
