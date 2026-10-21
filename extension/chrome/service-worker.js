/*!
 * RDM 下载接管 — Chrome MV3 service worker 引导脚本。
 *
 * Chrome 只允许 background.service_worker 指向一个脚本，而 background.js
 * 依赖加载顺序（compat → pure → background）。这里用 importScripts 保持与
 * Firefox 事件页（background.scripts 数组）一致的加载顺序。
 *
 * 由 `scripts/build-chrome-extension.ps1` 复制进 dist/rdm-chrome/；
 * 不要在此文件里写业务逻辑（业务代码在 background.js，Firefox/Chrome 共用）。
 */
"use strict";

importScripts("lib/compat.js", "lib/pure.js", "background.js");
