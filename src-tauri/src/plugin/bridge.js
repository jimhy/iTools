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
  var myPluginId = null; // 当前插件 id，供 settings.onChange 过滤（内部字段，不透给业务）

  // 事件总线：Rust 侧经 webview.eval 调 window.__itoolsEmit(channel, payload) 推送（热键/录制结束等）
  var eventCbs = {};
  window.__itoolsEmit = function (channel, payload) {
    (eventCbs[channel] || []).forEach(function (cb) {
      try { cb(payload); } catch (e) { console.error("[iTools] 事件回调异常", e); }
    });
  };
  function onChannel(channel, cb) {
    (eventCbs[channel] || (eventCbs[channel] = [])).push(cb);
  }

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
      if (p) {
        myPluginId = p.pluginId || null;
        // 只把 { code, type, query } 交给业务 onEnter；pluginId 是内部字段（供 settings.onChange 过滤）
        fireEnter({ code: p.code, type: p.type, query: p.query });
      }
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

  // base64 → ArrayBuffer（截图/读图的 IPC 载体是 base64 字符串，解回字节给插件）
  function b64ToBuf(b64) {
    var bin = atob(b64);
    var u8 = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    return u8.buffer;
  }
  // 图片数据（Uint8Array/ArrayBuffer/base64 字符串/data URL）→ base64（写图/贴图的 IPC 载体）
  function toImgB64(data) {
    if (typeof data === "string") {
      var comma = data.indexOf(",");
      return data.slice(0, 5) === "data:" && comma >= 0 ? data.slice(comma + 1) : data;
    }
    var u8 = data instanceof Uint8Array ? data : new Uint8Array(data);
    var bin = "";
    for (var i = 0; i < u8.length; i += 0x8000) bin += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
    return btoa(bin);
  }

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
    // 全局热键（需 hotkey 授权）：注册后按下即唤起本插件窗并触发 onHotkey 回调
    registerHotkey: function (accelerator, code) {
      return invoke("plugin_register_hotkey", {
        accelerator: String(accelerator),
        code: code != null ? String(code) : null,
      });
    },
    unregisterHotkey: function (accelerator) {
      return invoke("plugin_unregister_hotkey", { accelerator: String(accelerator) });
    },
    onHotkey: function (cb) {
      onChannel("hotkey", cb);
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
    // 剪贴板图片：readImage 读回 ArrayBuffer（PNG）；writeImage 接受 Uint8Array/ArrayBuffer/base64
    // 字符串（含 data URL），写入为真实图片。取代 base64-过-剪贴板-文本的老套路。
    readImage: function () {
      return invoke("plugin_read_image").then(b64ToBuf);
    },
    writeImage: function (data) {
      return invoke("plugin_write_image", { b64: toImgB64(data) });
    },
    // 保存图片：弹原生「另存为」，默认在「图片」目录。返回保存路径，取消返回 null
    saveImage: function (data, defaultName) {
      return invoke("plugin_save_image", { b64: toImgB64(data), defaultName: defaultName || null });
    },
    // 贴图：把图片钉成置顶浮窗（拖动/滚轮缩放/双击或 Esc 关闭/按 1 原始大小）。opacity 0.1~1，返回 pinId
    createPin: function (data, opacity) {
      return invoke("plugin_create_pin", { b64: toImgB64(data), opacity: opacity == null ? null : opacity });
    },
    // 离线 OCR（Windows.Media.Ocr）：识别图片中的文字。lang 可选（"zh-Hans"/"en"），返回文本
    ocr: function (data, lang) {
      return invoke("plugin_ocr", { b64: toImgB64(data), lang: lang || null });
    },
    // 录音（需 audio-capture 授权）：start 开始，stop 返回 ArrayBuffer(WAV)
    startAudioRecord: function () {
      return invoke("plugin_start_audio_record");
    },
    stopAudioRecord: function () {
      return invoke("plugin_stop_audio_record").then(b64ToBuf);
    },
    // 录屏 GIF（需 screen-capture 授权）：start 开始，stop 返回 ArrayBuffer(GIF)
    startGifRecord: function () {
      return invoke("plugin_start_gif_record");
    },
    stopGifRecord: function () {
      return invoke("plugin_stop_gif_record").then(b64ToBuf);
    },
    // 文件（writeFile 限定插件沙盒目录）
    readFile: function (path) {
      return invoke("plugin_read_file", { path: String(path) });
    },
    writeFile: function (path, content) {
      return invoke("plugin_write_file", { path: String(path), content: String(content) });
    },
    removeFile: function (path) {
      return invoke("plugin_remove_file", { path: String(path) });
    },
    // 截屏（需 screen-capture 授权）：captureFull 返回 ArrayBuffer(PNG)；listDisplays 返回显示器数组
    listDisplays: function () {
      return invoke("plugin_list_displays");
    },
    captureFull: function (displayId) {
      return invoke("plugin_capture_full", { displayId: displayId == null ? null : displayId }).then(b64ToBuf);
    },
    // PixPin 风格区域截图：隐藏面板→冻结屏→覆盖层里框选+就地标注+悬浮工具栏→用户点 复制/保存/贴图/OCR。
    // opts.full=true 则开局选中整屏。返回 { action, image:ArrayBuffer(PNG) }；用户取消返回 null。
    captureRegion: function (opts) {
      opts = opts || {};
      return invoke("plugin_capture_region", { full: !!opts.full }).then(
        function (res) {
          return { action: res.action, image: b64ToBuf(res.b64) };
        },
        function (err) {
          if (String(err).indexOf("__cancelled__") >= 0) return null;
          throw err;
        }
      );
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
    // 账号态（只读；仅暴露 loggedIn/cloudConfigured/syncEnabled，不含用户名/token）
    account: {
      // { loggedIn, cloudConfigured, syncEnabled }
      state: function () {
        return invoke("plugin_account_state");
      },
      // 便捷：是否已登录云账号
      isLoggedIn: function () {
        return invoke("plugin_account_state").then(function (s) {
          return !!(s && s.loggedIn);
        });
      },
    },
    // 本地优先数据（写入先落本地；已登录 + 云端已配置时经 sync() 上行云端，否则诚实返回 reason）
    // value 自动 JSON 序列化，与 db 一致；与 db 的区别是 data 参与云同步、db 纯本地。
    data: {
      get: function (key) {
        return invoke("plugin_data_get", { key: String(key) }).then(function (v) {
          return v == null ? null : JSON.parse(v);
        });
      },
      set: function (key, value) {
        return invoke("plugin_data_set", { key: String(key), value: JSON.stringify(value) });
      },
      remove: function (key) {
        return invoke("plugin_data_remove", { key: String(key) });
      },
      keys: function (prefix) {
        return invoke("plugin_data_keys", { prefix: prefix ? String(prefix) : null });
      },
      // 手动触发同步到云端：{ synced, reason?, pushed, pulled, message? }
      // reason 可能为 cloud_not_configured / not_logged_in / offline / error
      sync: function () {
        return invoke("plugin_data_sync");
      },
    },
    // 设置（只读）：读用户在 iTools「插件管理 → 本插件 → 设置」里配置的值。
    // schema 由插件目录的 settings.json 声明；值 = schema 默认 + 用户覆盖，由管理中心写入，插件只读。
    settings: {
      // 读单项（不存在返回 null）
      get: function (key) {
        return invoke("plugin_get_setting", { key: String(key) });
      },
      // 读全部：{ key: value, ... }
      all: function () {
        return invoke("plugin_get_settings");
      },
      // 用户在管理中心改了本插件设置时回调，cb 收到最新全量设置对象
      onChange: function (cb) {
        onChannel("settings-changed", function (changedId) {
          if (myPluginId && changedId !== myPluginId) return;
          invoke("plugin_get_settings").then(function (s) {
            try {
              cb(s);
            } catch (e) {
              console.error("[iTools] settings.onChange 回调异常", e);
            }
          });
        });
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
  Object.freeze(itools.account);
  Object.freeze(itools.data);
  Object.freeze(itools.settings);
  Object.freeze(itools.platform);
  Object.freeze(itools);
  Object.defineProperty(window, "itools", { value: itools, writable: false, configurable: false });
})();
