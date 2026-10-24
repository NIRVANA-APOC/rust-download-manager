/*!
 * Chrome 兼容层 lib/compat.js 的单元测试（Node 内置 test runner）。
 *
 *   node --test extension/chrome/test/compat.test.js
 *
 * 覆盖：chrome.* 回调 → Promise 包装、runtime.lastError 拒绝、
 * runtime.onMessage 返回 Promise 的监听器、alarms.create 的两种实现、
 * connectNative / 事件对象透传、Firefox 环境不覆盖 browser。
 */
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const COMPAT_PATH = require.resolve("../lib/compat.js");

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

function createChromeStub() {
  const events = {
    onMessage: createEvent(),
    onInstalled: createEvent(),
    onStartup: createEvent(),
    webRequest: {
      onBeforeRequest: createEvent(),
      onBeforeSendHeaders: createEvent(),
      onHeadersReceived: createEvent(),
      onBeforeRedirect: createEvent(),
    },
    onCreated: createEvent(),
    onChanged: createEvent(),
    onClicked: createEvent(),
    onAlarm: createEvent(),
  };
  const calls = { setBadgeText: [], setBadgeTextColor: [], menuCreate: [], executeScript: [], tabsCreate: [] };
  const chromeApi = {
    runtime: {
      id: "abcdefghijklmnopabcdefghijklmnop",
      lastError: null,
      getManifest: () => ({ version: "0.1.0", manifest_version: 3 }),
      sendMessage: (message, callback) => {
        setTimeout(() => callback({ ok: true, echoed: message }), 0);
      },
      openOptionsPage: (callback) => callback(),
      connectNative: (name) => ({ name, posted: [] }),
      onMessage: events.onMessage,
      onInstalled: events.onInstalled,
      onStartup: events.onStartup,
    },
    storage: {
      local: {
        get: (key, callback) => callback({ [key]: "stored-" + key }),
        set: (values, callback) => callback(),
      },
      onChanged: events.onChanged,
    },
    cookies: {
      getAll: (details, callback) => callback([{ name: "sid", value: "1" }]),
    },
    tabs: {
      query: (query, callback) => callback([{ id: 1, url: "https://example.com/" }]),
      create: (options, callback) => {
        calls.tabsCreate.push(options);
        callback({ id: 99 });
      },
    },
    scripting: {
      executeScript: (details, callback) => {
        calls.executeScript.push(details);
        setTimeout(() => callback([{ frameId: 0, result: { links: 2 } }]), 0);
      },
    },
    downloads: {
      onCreated: events.onCreated,
      onChanged: events.onChanged,
      cancel: (id, callback) => callback(),
      erase: (query, callback) => callback([1]),
      removeFile: (id, callback) => callback(),
      search: (query, callback) => callback([{ id: 1, state: "in_progress" }]),
      download: (options, callback) => callback(42),
    },
    contextMenus: {
      onClicked: events.onClicked,
      create: (props, callback) => {
        calls.menuCreate.push(props);
        if (callback) callback();
        return props.id;
      },
      remove: (id, callback) => callback(),
    },
    alarms: {
      onAlarm: events.onAlarm,
      get: (name, callback) => callback(undefined),
      create: (name, info, callback) => {
        if (callback) callback();
      },
    },
    action: {
      setBadgeText: (details, callback) => {
        calls.setBadgeText.push(details);
        if (callback) callback();
      },
      setBadgeBackgroundColor: (details, callback) => {
        if (callback) callback();
      },
      setBadgeTextColor: (details, callback) => {
        calls.setBadgeTextColor.push(details);
        if (callback) callback();
      },
      setTitle: (details, callback) => {
        if (callback) callback();
      },
    },
    webRequest: events.webRequest,
  };
  return { chromeApi, events, calls };
}

function loadCompat(chromeApi) {
  delete require.cache[COMPAT_PATH];
  delete globalThis.browser;
  globalThis.chrome = chromeApi;
  require(COMPAT_PATH);
  return globalThis.browser;
}

test.afterEach(() => {
  delete globalThis.browser;
  delete globalThis.chrome;
});

test("已存在 browser（Firefox）时不做任何覆盖", () => {
  const sentinel = { runtime: { connectNative: () => ({}) } };
  delete require.cache[COMPAT_PATH];
  delete globalThis.chrome;
  globalThis.browser = sentinel;
  require(COMPAT_PATH);
  assert.equal(globalThis.browser, sentinel);
});

test("存储 / runtime / tabs / cookies / downloads / action 回调包装为 Promise", async () => {
  const { chromeApi, calls } = createChromeStub();
  const browser = loadCompat(chromeApi);

  assert.deepEqual(await browser.storage.local.get("rdm.settings"), {
    "rdm.settings": "stored-rdm.settings",
  });
  await browser.storage.local.set({ a: 1 });

  assert.deepEqual(await browser.runtime.sendMessage({ x: 1 }), {
    ok: true,
    echoed: { x: 1 },
  });
  assert.deepEqual(await browser.tabs.query({ active: true }), [
    { id: 1, url: "https://example.com/" },
  ]);
  assert.deepEqual(await browser.cookies.getAll({ url: "https://example.com/" }), [
    { name: "sid", value: "1" },
  ]);

  assert.equal(await browser.downloads.download({ url: "https://example.com/a" }), 42);
  assert.deepEqual(await browser.downloads.search({ id: 1 }), [
    { id: 1, state: "in_progress" },
  ]);
  await browser.downloads.cancel(1);
  await browser.downloads.removeFile(1);
  assert.deepEqual(await browser.downloads.erase({ id: 1 }), [1]);

  await browser.contextMenus.create({ id: "menu-1", title: "x" });
  assert.equal(calls.menuCreate.length, 1);
  await browser.runtime.openOptionsPage();
  await browser.action.setBadgeText({ text: "ON" });
  assert.deepEqual(calls.setBadgeText, [{ text: "ON" }]);
  assert.equal(browser.runtime.id, "abcdefghijklmnopabcdefghijklmnop");
  assert.deepEqual(browser.runtime.getManifest(), {
    version: "0.1.0",
    manifest_version: 3,
  });
});

test("runtime.lastError 会让 Promise 拒绝，getter 也能读到", async () => {
  const { chromeApi } = createChromeStub();
  chromeApi.runtime.sendMessage = (message, callback) => {
    chromeApi.runtime.lastError = {
      message: "Could not establish connection. Receiving end does not exist.",
    };
    callback(undefined);
    chromeApi.runtime.lastError = null;
  };
  const browser = loadCompat(chromeApi);

  await assert.rejects(
    browser.runtime.sendMessage({}),
    /Receiving end does not exist/
  );

  chromeApi.runtime.lastError = { message: "boom" };
  assert.deepEqual(browser.runtime.lastError, { message: "boom" });
  chromeApi.runtime.lastError = null;
  assert.equal(browser.runtime.lastError, null);
});

test("runtime.onMessage 支持返回 Promise 的监听器（sendResponse + return true）", async () => {
  const { chromeApi, events } = createChromeStub();
  const browser = loadCompat(chromeApi);

  browser.runtime.onMessage.addListener(async (message) => ({
    ok: true,
    echoed: message.type,
  }));
  assert.equal(events.onMessage.listeners.length, 1);
  const wrapped = events.onMessage.listeners[0];

  let response;
  const keepAlive = wrapped(
    { type: "getState" },
    {},
    (value) => {
      response = value;
    }
  );
  assert.equal(keepAlive, true, "异步监听器必须返回 true 保持通道");
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(response, { ok: true, echoed: "getState" });

  // 同步监听器原样返回对象。
  browser.runtime.onMessage.addListener((message) => ({ ok: false, error: "sync" }));
  const syncWrapped = events.onMessage.listeners[1];
  assert.deepEqual(syncWrapped({}, {}, () => {}), { ok: false, error: "sync" });

  // 抛异常的监听器转换成结构化错误，而不是让通道静默断开。
  browser.runtime.onMessage.addListener(() => {
    throw new Error("nope");
  });
  const throwingWrapped = events.onMessage.listeners[2];
  let captured;
  throwingWrapped({}, {}, (value) => {
    captured = value;
  });
  assert.deepEqual(captured, { ok: false, error: "Error: nope" });
});

test("alarms.create 同时兼容回调风格与 Promise 风格实现", async () => {
  const callbackStub = createChromeStub();
  const browser = loadCompat(callbackStub.chromeApi);
  await browser.alarms.create("rdm.status.ping", { periodInMinutes: 5 });
  assert.equal(await browser.alarms.get("rdm.status.ping"), undefined);

  const promiseStub = createChromeStub();
  promiseStub.chromeApi.alarms.create = () => Promise.resolve();
  const promiseBrowser = loadCompat(promiseStub.chromeApi);
  await promiseBrowser.alarms.create("rdm.status.ping", { periodInMinutes: 5 });
});

test("scripting.executeScript 委托 chrome.scripting：details 透传、回调 → Promise", async () => {
  const { chromeApi, calls } = createChromeStub();
  const browser = loadCompat(chromeApi);
  const details = {
    target: { tabId: 7, allFrames: false },
    func: () => ({ links: 2 }),
  };
  const result = await browser.scripting.executeScript(details);
  assert.deepEqual(result, [{ frameId: 0, result: { links: 2 } }]);
  assert.equal(calls.executeScript.length, 1);
  assert.equal(calls.executeScript[0], details, "target/func 必须原样透传");
});

test("scripting.executeScript 的 runtime.lastError → Promise reject", async () => {
  const { chromeApi } = createChromeStub();
  chromeApi.scripting.executeScript = (_details, callback) => {
    chromeApi.runtime.lastError = {
      message: "Cannot access contents of the page",
    };
    callback(undefined);
    chromeApi.runtime.lastError = null;
  };
  const browser = loadCompat(chromeApi);
  await assert.rejects(
    browser.scripting.executeScript({ target: { tabId: 1 } }),
    /Cannot access contents of the page/
  );
});

test("缺少 chrome.scripting 时 executeScript 返回 rejected Promise（不抛同步异常）", async () => {
  const { chromeApi } = createChromeStub();
  delete chromeApi.scripting;
  const browser = loadCompat(chromeApi);
  await assert.rejects(
    browser.scripting.executeScript({ target: { tabId: 1 } }),
    /不可用/
  );
});

test("connectNative 原样返回 Port，事件对象透传且不丢监听器", () => {
  const { chromeApi, events } = createChromeStub();
  const browser = loadCompat(chromeApi);

  const port = browser.runtime.connectNative("com.rdm.download");
  assert.equal(port.name, "com.rdm.download");

  assert.equal(browser.downloads.onCreated, events.onCreated);
  assert.equal(browser.contextMenus.onClicked, events.onClicked);
  assert.equal(browser.alarms.onAlarm, events.onAlarm);
  assert.equal(browser.webRequest.onBeforeRequest, events.webRequest.onBeforeRequest);
  assert.equal(browser.storage.onChanged, events.onChanged);

  const listener = () => {};
  browser.webRequest.onBeforeSendHeaders.addListener(listener, { urls: ["<all_urls>"] }, [
    "requestHeaders",
  ]);
  assert.equal(events.webRequest.onBeforeSendHeaders.listeners[0], listener);
});

test("非扩展环境（没有 chrome API）不定义 browser", () => {
  delete require.cache[COMPAT_PATH];
  delete globalThis.browser;
  globalThis.chrome = {};
  require(COMPAT_PATH);
  assert.equal(globalThis.browser, undefined);
});

test("tabs.create 与 action.setBadgeTextColor 回调包装为 Promise（M4 popup/options 新增）", async () => {
  const { chromeApi, calls } = createChromeStub();
  const browser = loadCompat(chromeApi);

  const created = await browser.tabs.create({ url: "moz-extension://test/options.html#help" });
  assert.deepEqual(created, { id: 99 });
  assert.equal(calls.tabsCreate.length, 1);
  assert.equal(calls.tabsCreate[0].url, "moz-extension://test/options.html#help");

  await browser.action.setBadgeTextColor({ color: "#FFFFFF" });
  assert.deepEqual(calls.setBadgeTextColor, [{ color: "#FFFFFF" }]);
});

test("旧 Chrome 没有 action.setBadgeTextColor 时降级为 resolved Promise", async () => {
  const { chromeApi } = createChromeStub();
  delete chromeApi.action.setBadgeTextColor;
  const browser = loadCompat(chromeApi);
  await browser.action.setBadgeTextColor({ color: "#FFFFFF" });
});
