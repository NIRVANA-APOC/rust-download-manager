/*!
 * RDM 下载接管 — Chrome MV3 兼容层（`chrome.*` 回调风格 → Promise 风格）。
 *
 * Firefox 提供 `browser.*`（Promise）；Chrome MV3 只提供 `chrome.*`，且多数
 * 方法仍以「最后一个参数是回调」为主。本文件在 Chrome 中定义 `globalThis.browser`，
 * 包装 background.js / popup.js / options.js 实际用到的 API：
 *
 *   runtime   getManifest / id / lastError / sendMessage / openOptionsPage /
 *             connectNative / onMessage（支持返回 Promise 的监听器）/
 *             onInstalled / onStartup
 *   storage   local.get / local.set / onChanged
 *   cookies   getAll
 *   tabs      query
 *   scripting executeScript（页面链接快照采集；回调 → Promise）
 *   downloads cancel / erase / removeFile / search / download / onCreated / onChanged
 *   contextMenus create / remove / onClicked
 *   alarms    get / create / onAlarm
 *   action    setBadgeText / setBadgeBackgroundColor / setTitle
 *   webRequest onBeforeRequest / onBeforeSendHeaders / onHeadersReceived / onBeforeRedirect
 *
 * 在 Firefox（已存在 `browser`）中本文件不做任何事，因此同一份扩展源码可以
 * 直接用于两种浏览器。
 */
(function (global) {
  "use strict";

  if (
    global.browser &&
    global.browser.runtime &&
    typeof global.browser.runtime.connectNative === "function"
  ) {
    // Firefox：保留原生 browser.* 实现。
    return;
  }

  const chromeApi = global.chrome;
  if (!chromeApi || !chromeApi.runtime) {
    // 非扩展环境：不定义 browser，让调用方得到可读的 ReferenceError。
    return;
  }

  function currentLastError() {
    return chromeApi.runtime && chromeApi.runtime.lastError
      ? chromeApi.runtime.lastError
      : null;
  }

  function toError(error) {
    const message = error && error.message ? error.message : String(error);
    return new Error(message);
  }

  /** 回调风格 API → Promise，resolve 回调结果（get/query/search/download/…）。 */
  function callbackResult(target, method) {
    return function (...args) {
      return new Promise((resolve, reject) => {
        try {
          target[method].apply(target, [
            ...args,
            (result) => {
              const error = currentLastError();
              if (error) reject(toError(error));
              else resolve(result);
            },
          ]);
        } catch (err) {
          reject(toError(err));
        }
      });
    };
  }

  /** 回调风格 API → Promise，忽略回调结果（set/cancel/openOptionsPage/…）。 */
  function callbackDone(target, method) {
    return function (...args) {
      return new Promise((resolve, reject) => {
        try {
          target[method].apply(target, [
            ...args,
            () => {
              const error = currentLastError();
              if (error) reject(toError(error));
              else resolve();
            },
          ]);
        } catch (err) {
          reject(toError(err));
        }
      });
    };
  }

  /**
   * 既支持回调、也可能直接返回 Promise 的 void API（如 chrome.alarms.create）。
   * 优先等待回调；若实现不调用回调而返回 Promise，则跟随该 Promise；
   * 两者都没有时在下一微任务视为完成，保证 `await` 不会永久挂起。
   */
  function callbackOrPromise(target, method) {
    return function (...args) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const done = () => {
          if (settled) return;
          settled = true;
          const error = currentLastError();
          if (error) reject(toError(error));
          else resolve();
        };
        let returned;
        try {
          returned = target[method].apply(target, [...args, done]);
        } catch (err) {
          reject(toError(err));
          return;
        }
        if (returned && typeof returned.then === "function") {
          returned.then(done, (err) => {
            if (settled) return;
            settled = true;
            reject(toError(err));
          });
        } else {
          queueMicrotask(done);
        }
      });
    };
  }

  /** chrome 事件对象与 browser 事件对象同构，原样透传即可。 */
  function passthroughEvent(event) {
    return event;
  }

  /**
   * runtime.onMessage：Chrome MV3 对「监听器返回 Promise」的支持因版本而异，
   * 这里统一改成「返回 true + sendResponse」的回调协议。
   */
  function wrapMessageEvent(event) {
    return {
      addListener(listener) {
        event.addListener((message, sender, sendResponse) => {
          let result;
          try {
            result = listener(message, sender, sendResponse);
          } catch (err) {
            sendResponse({ ok: false, error: String(err) });
            return undefined;
          }
          if (result && typeof result.then === "function") {
            result.then(
              (value) => sendResponse(value === undefined ? { ok: true } : value),
              (err) => sendResponse({ ok: false, error: String(err) })
            );
            return true; // 保持消息通道打开，等待异步响应
          }
          return result;
        });
      },
      removeListener(listener) {
        event.removeListener(listener);
      },
      hasListener(listener) {
        return event.hasListener(listener);
      },
    };
  }

  const runtime = {
    id: chromeApi.runtime.id,
    getManifest: () => chromeApi.runtime.getManifest(),
    connectNative: (name) => chromeApi.runtime.connectNative(name),
    sendMessage: callbackResult(chromeApi.runtime, "sendMessage"),
    openOptionsPage: callbackOrPromise(chromeApi.runtime, "openOptionsPage"),
    onMessage: wrapMessageEvent(chromeApi.runtime.onMessage),
    onInstalled: passthroughEvent(chromeApi.runtime.onInstalled),
    onStartup: passthroughEvent(chromeApi.runtime.onStartup),
  };
  Object.defineProperty(runtime, "lastError", {
    enumerable: true,
    get() {
      const error = currentLastError();
      return error ? { message: error.message || String(error) } : null;
    },
  });

  const storage = {
    local: {
      get: callbackResult(chromeApi.storage.local, "get"),
      set: callbackDone(chromeApi.storage.local, "set"),
    },
    onChanged: passthroughEvent(chromeApi.storage.onChanged),
  };

  const downloads = {
    cancel: callbackDone(chromeApi.downloads, "cancel"),    erase: callbackResult(chromeApi.downloads, "erase"),
    removeFile: callbackDone(chromeApi.downloads, "removeFile"),
    search: callbackResult(chromeApi.downloads, "search"),
    download: callbackResult(chromeApi.downloads, "download"),
    onCreated: passthroughEvent(chromeApi.downloads.onCreated),
    onChanged: passthroughEvent(chromeApi.downloads.onChanged),
  };

  const contextMenus = {
    create: callbackDone(chromeApi.contextMenus, "create"),
    remove: callbackDone(chromeApi.contextMenus, "remove"),
    onClicked: passthroughEvent(chromeApi.contextMenus.onClicked),
  };

  const alarms = {
    get: callbackResult(chromeApi.alarms, "get"),
    create: callbackOrPromise(chromeApi.alarms, "create"),
    onAlarm: passthroughEvent(chromeApi.alarms.onAlarm),
  };

  const action = {
    setBadgeText: callbackDone(chromeApi.action, "setBadgeText"),
    setBadgeBackgroundColor: callbackDone(chromeApi.action, "setBadgeBackgroundColor"),
    setTitle: callbackDone(chromeApi.action, "setTitle"),
  };

  /**
   * scripting.executeScript：Chrome 回调风格 → Promise。`details` 原样透传
   * （target/func/args），resolve 注入结果数组（InjectionResult[]）。
   * 旧版 Chrome/无该 API 时返回 rejected Promise，而不是抛同步异常。
   */
  const scripting = {
    executeScript: (...args) => {
      if (!chromeApi.scripting || typeof chromeApi.scripting.executeScript !== "function") {
        return Promise.reject(new Error("chrome.scripting.executeScript 不可用"));
      }
      return callbackResult(chromeApi.scripting, "executeScript")(...args);
    },
  };

  global.browser = {
    runtime,
    storage,
    cookies: { getAll: callbackResult(chromeApi.cookies, "getAll") },
    tabs: { query: callbackResult(chromeApi.tabs, "query") },
    scripting,
    downloads,
    contextMenus,
    alarms,
    action,
    webRequest: {
      onBeforeRequest: passthroughEvent(chromeApi.webRequest.onBeforeRequest),
      onBeforeSendHeaders: passthroughEvent(chromeApi.webRequest.onBeforeSendHeaders),
      onHeadersReceived: passthroughEvent(chromeApi.webRequest.onHeadersReceived),
      onBeforeRedirect: passthroughEvent(chromeApi.webRequest.onBeforeRedirect),
    },
  };
})(typeof globalThis !== "undefined" ? globalThis : this);
