// iTools 插件桥接：作为 WebviewWindow 的 initialization_script 在插件页任何脚本前执行，
// 构造受控的 window.itools 门面。所有能力经 __TAURI_INTERNALS__.invoke 转发到后端 plugin_* 白名单命令。
// 真正的安全边界是后端 capability 白名单，本门面只是便利层。
(function () {
  "use strict";
  var internals = window.__TAURI_INTERNALS__;
  function invoke(cmd, args) {
    if (!internals || !internals.invoke) {
      return Promise.reject(new Error("iTools IPC 不可用"));
    }
    return internals.invoke(cmd, args || {});
  }

  var enterCbs = [];
  var exitCbs = [];
  var enterPayload = null;

  function fireEnter(p) {
    enterPayload = p;
    enterCbs.forEach(function (cb) {
      try {
        cb(p);
      } catch (e) {
        console.error("[iTools] onEnter 回调异常", e);
      }
    });
  }

  // 拉取本次进入信息（避免 emit 与页面监听的时序竞态）：谁先就绪都能拿到。
  invoke("plugin_take_enter")
    .then(function (p) {
      if (p) fireEnter(p);
    })
    .catch(function () {});

  // 退出：页面卸载/隐藏时触发已注册回调（纯前端近似）。
  window.addEventListener("pagehide", function () {
    exitCbs.forEach(function (cb) {
      try {
        cb();
      } catch (e) {}
    });
  });

  // --- 轻量 Toast（纯前端 DOM，无需后端）---
  function showToast(msg) {
    var el = document.createElement("div");
    el.textContent = msg;
    el.style.cssText =
      "position:fixed;left:50%;bottom:28px;transform:translateX(-50%);" +
      "background:rgba(30,30,32,.92);color:#fff;padding:9px 16px;border-radius:10px;" +
      "font:13px/1.4 system-ui,'Segoe UI',sans-serif;z-index:2147483647;pointer-events:none;" +
      "box-shadow:0 6px 24px rgba(0,0,0,.28);opacity:0;transition:opacity .18s";
    document.body.appendChild(el);
    requestAnimationFrame(function () {
      el.style.opacity = "1";
    });
    setTimeout(function () {
      el.style.opacity = "0";
      setTimeout(function () {
        el.remove();
      }, 220);
    }, 1600);
  }

  var itools = {
    // 生命周期
    onEnter: function (cb) {
      enterCbs.push(cb);
      if (enterPayload) {
        try {
          cb(enterPayload);
        } catch (e) {
          console.error(e);
        }
      }
    },
    onExit: function (cb) {
      exitCbs.push(cb);
    },
    // 窗口
    hide: function () {
      return invoke("plugin_hide");
    },
    exit: function () {
      return invoke("plugin_exit");
    },
    setHeight: function (px) {
      return invoke("plugin_set_height", { height: Math.round(px) });
    },
    // 剪贴板
    copyText: function (text) {
      return invoke("plugin_copy_text", { text: String(text) });
    },
    readText: function () {
      return invoke("plugin_read_text");
    },
    // 文件（writeFile 限定插件沙盒目录）
    readFile: function (path) {
      return invoke("plugin_read_file", { path: String(path) });
    },
    writeFile: function (path, content) {
      return invoke("plugin_write_file", { path: String(path), content: String(content) });
    },
    // 系统
    openExternal: function (url) {
      return invoke("plugin_open_external", { url: String(url) });
    },
    openPath: function (path) {
      return invoke("plugin_open_path", { path: String(path) });
    },
    notify: function (body) {
      return invoke("plugin_notify", { body: String(body) });
    },
    runCommand: function (program, args) {
      return invoke("plugin_run_command", {
        program: String(program),
        args: (args || []).map(String),
      });
    },
    // 联网（需授权 network）：经原生代理，返回 { status, ok, body }（文本）
    fetch: function (url, init) {
      init = init || {};
      return invoke("plugin_fetch", {
        url: String(url),
        method: init.method || "GET",
        headers: init.headers || null,
        body: init.body != null ? String(init.body) : null,
      });
    },
    // 存储（KV，value 自动 JSON 序列化）
    db: {
      get: function (key) {
        return invoke("plugin_db_get", { key: String(key) }).then(function (v) {
          return v == null ? null : JSON.parse(v);
        });
      },
      set: function (key, value) {
        return invoke("plugin_db_set", { key: String(key), value: JSON.stringify(value) });
      },
      remove: function (key) {
        return invoke("plugin_db_remove", { key: String(key) });
      },
      keys: function (prefix) {
        return invoke("plugin_db_keys", { prefix: prefix ? String(prefix) : null });
      },
    },
    // UI
    showToast: function (msg) {
      showToast(String(msg));
    },
    // 平台
    platform: {
      isWindows: true,
      isMacOS: false,
      isLinux: false,
      isDev: !!window.__ITOOLS_DEV__,
    },
  };

  Object.freeze(itools.db);
  Object.freeze(itools.platform);
  Object.freeze(itools);
  Object.defineProperty(window, "itools", { value: itools, writable: false, configurable: false });
})();
