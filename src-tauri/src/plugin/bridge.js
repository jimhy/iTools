// iTools 插件桥接：作为 WebviewWindow 的 initialization_script 在插件页任何脚本前执行，
// 构造受控的 window.itools 门面。所有能力经 __TAURI_INTERNALS__.invoke 转发到后端 plugin_* 白名单命令。
// 真正的安全边界是后端 capability 白名单，本门面只是便利层。
//
// 调试探针：**只在插件调试窗口**（Rust 侧 dev::commands::open_dev_window 注入
// window.__ITOOLS_DEBUG__=true）挂载，把门面调用/console/未捕获异常记进开发者中心的日志时间线。
// 正式插件窗口只做一次布尔判断，之后一行探针代码都不执行。实现见文件末尾 installProbe()。
(function () {
  "use strict";
  var internals = window.__TAURI_INTERNALS__;

  // 原生 IPC 直通。**调试探针的上报专用通道**：上报若走下面的 invoke，会被探针自己记成一条
  // 新日志 → 又触发一次上报 → 无限自激刷屏。所以 dev_log_push 必须从这里发。
  //
  // ⚠ 为什么不干脆钩住 __TAURI_INTERNALS__.invoke（那样连绕过门面的直调也能抓到）：
  // Tauri 2.11 的 core.js 用 `Object.defineProperty(internals, "invoke", { value })` 定义它，
  // 描述符默认 writable:false / configurable:false —— 严格模式下赋值直接抛 TypeError，
  // defineProperty 也会失败；internals.ipc / internals.postMessage 同样如此。
  // 硬钩只会把「正在被调试的运行时」本身搞坏，因此**明确接受这个盲区**，并在 probe.ready
  // 日志与管理中心面板里如实写明：「日志里没有」≠「没发生」。
  function rawInvoke(cmd, args) {
    if (!internals || !internals.invoke) {
      return Promise.reject(new Error("iTools IPC 不可用"));
    }
    return internals.invoke(cmd, args || {});
  }

  // 门面统一出口：所有 itools.* 能力都经它转发——探针的唯一注入点。
  var invoke = rawInvoke;

  // 调试探针实例（正式插件窗口恒为 null）。必须在**任何** invoke 调用之前装好，
  // 否则下面 plugin_take_enter 那一发就漏记了。
  var probe = null;
  if (window.__ITOOLS_DEBUG__ === true) installProbe();

  var enterCbs = [];
  var exitCbs = [];
  var enterPayload = null;
  var myPluginId = null; // 当前插件 id，供 settings.onChange 过滤（内部字段，不透给业务）

  // 事件总线：Rust 侧经 webview.eval 调 window.__itoolsEmit(channel, payload) 推送（热键/录制结束等）
  var eventCbs = {};
  window.__itoolsEmit = function (channel, payload) {
    // 调试窗口：后端推来的事件也记进时间线（否则「热键按了没反应」查不到是没推来还是回调抛了）。
    // 正式窗口 probe 恒为 null，这里只是一次判空。
    if (probe) probe.event(channel, payload);
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
    // 读取本地图片文件（png/jpg/…）→ ArrayBuffer。供把外部图片本地化前取字节（文件路径粘贴 / 资源管理器拖入路径）。
    readLocalImage: function (path) {
      return invoke("plugin_read_local_image", { path: String(path) }).then(b64ToBuf);
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
      // reason 可能为 cloud_not_configured / not_logged_in / offline / session_expired / error
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
  // 必须用普通属性赋值挂载，不能 defineProperty(writable:false/configurable:false)：
  // 全局对象上的不可配置属性会触发 ES 规范 HasRestrictedGlobalProperty 限制——插件页顶层一句
  // `const itools = window.itools;` 就会在脚本实例化阶段抛 SyntaxError，整个 <script> 零执行
  // （deskbox 曾因此所有按钮失灵）；writable:false 则让 `window.itools = mock` 在严格模式下抛
  // TypeError。防篡改在此无安全意义：安全边界是后端 capability 白名单，页面本就能绕过门面
  // 直用 __TAURI_INTERNALS__。对象本身已 freeze，足以防误改 API 表面。
  window.itools = itools;

  // ==================================================================================
  // 调试探针（仅插件调试窗口）
  // ==================================================================================
  // 设计约束（按重要度）：
  // 1. **不污染被测对象**：console 的原始行为完整保留且先于记录执行；异常监听用
  //    addEventListener 而非覆盖 window.onerror（不抢插件自己的处理器）；invoke 只观察不改写，
  //    返回给插件的仍是原始 promise，成功值与错误值一字不动。
  // 2. **不自我递归**：上报走 rawInvoke（绕过本探针），失败只用**原始** console 提醒一次；
  //    探针内部一律不调用被钩住的 console。
  // 3. **不爆内存**：二进制/base64（截图、录音、贴图、剪贴板图）只记类型与字节数，绝不整块入库；
  //    普通字符串截到 200 字符，单条 args/result 总长截到 1200（后端还会在 2000 处再截一刀）。
  // 4. **不影响插件运行**：全程 try/catch 兜底，探针出错就静默放弃这条日志。
  //
  // 覆盖范围与盲区见 probe.ready 那条日志——它自己就写在时间线里，开发者一眼可见。
  function installProbe() {
    try {
      // ---------- 截断策略 ----------
      var MAX_STR = 200; // 普通字符串保留的字符数
      var BIN_STR = 4096; // 超过它一律按二进制/base64 处理：只记类型与体量，不记内容
      var MAX_TEXT = 1200; // 单条 args / result 的总长上限
      var MAX_KEYS = 24; // 对象最多记多少个字段
      var MAX_ITEMS = 20; // 数组 / 参数列表最多记多少项
      var MAX_DEPTH = 3; // 递归深度（超出只记类型；顺带天然规避循环引用）
      // ---------- 上报节流 ----------
      var BATCH = 20; // 攒满立刻发
      var FLUSH_MS = 100; // 否则最多攒这么久
      var QUEUE_MAX = 500; // 上报通道堵住时的自保上限（超出丢最旧，并如实记一条）

      var queue = [];
      var timer = null;
      var sending = false;
      var dropped = 0;
      var warnedPushFail = false;
      var origWarn = null; // 未被钩住的原始 console.warn（探针自身唯一允许的输出口）

      var nowMs =
        window.performance && typeof window.performance.now === "function"
          ? function () { return window.performance.now(); }
          : function () { return Date.now(); };

      function clip(s) {
        s = String(s);
        return s.length > MAX_TEXT ? s.slice(0, MAX_TEXT) + "…（已截断）" : s;
      }

      // base64 字符数 → 原始字节数（末尾 '=' 是填充）。只为在日志里给出体量，不做解码。
      function b64Bytes(s) {
        var n = s.length, pad = 0;
        if (n > 0 && s.charCodeAt(n - 1) === 61) pad++;
        if (n > 1 && s.charCodeAt(n - 2) === 61) pad++;
        return Math.max(0, Math.floor(n / 4) * 3 - pad);
      }

      // 只采样前 64 个字符判断「像不像 base64」：几 MB 的字符串上跑正则太贵。
      var B64_HEAD = /^[A-Za-z0-9+/=\r\n]+$/;
      function looksBinary(s) {
        return B64_HEAD.test(s.slice(0, 64));
      }

      // 字符串摘要。二进制 / base64 只记「类型 + 体量」，绝不记内容：
      // 一次截图/录音就是几 MB base64，整块塞进日志会直接把内存和 UI 打垮。
      // 普通长文本（如 writeFile 的文本内容）则保留前 200 字符——那对调试有用，
      // 而且**不能**把它谎报成「二进制」。两条路径都只 slice 200 字符，不复制大字符串。
      function briefStr(s, binary) {
        var n = s.length;
        if (binary || (n > BIN_STR && looksBinary(s))) {
          return "«二进制/base64：" + n + " 字符 ≈ " + b64Bytes(s) + " 字节，内容未记录»";
        }
        if (n > MAX_STR) return s.slice(0, MAX_STR) + "…（已截断，共 " + n + " 字符）";
        return s;
      }

      function brief(v, depth, key) {
        if (v === null || v === undefined) return null;
        var t = typeof v;
        if (t === "number") return isFinite(v) ? v : String(v);
        if (t === "boolean") return v;
        if (t === "string") return briefStr(v, key === "b64"); // b64 是所有图片/音频入参的字段名
        if (t === "function") return "«function»";
        if (t === "symbol" || t === "bigint") return String(v);
        if (v instanceof Error) return String(v.name) + ": " + briefStr(String(v.message || ""), false);
        if (typeof ArrayBuffer !== "undefined") {
          if (v instanceof ArrayBuffer) return "«ArrayBuffer " + v.byteLength + " 字节»";
          if (ArrayBuffer.isView(v)) {
            return "«" + ((v.constructor && v.constructor.name) || "TypedArray") + " " + v.byteLength + " 字节»";
          }
        }
        if (typeof Blob !== "undefined" && v instanceof Blob) return "«Blob " + v.size + " 字节»";
        if (depth >= MAX_DEPTH) return Array.isArray(v) ? "«Array(" + v.length + ")»" : "«Object»";
        if (Array.isArray(v)) {
          var arr = [];
          for (var i = 0; i < v.length && i < MAX_ITEMS; i++) arr.push(pick(v, i, depth + 1));
          if (v.length > MAX_ITEMS) arr.push("…（共 " + v.length + " 项）");
          return arr;
        }
        var o = {}, n = 0;
        for (var k in v) {
          if (!Object.prototype.hasOwnProperty.call(v, k)) continue;
          if (n++ >= MAX_KEYS) { o["…"] = "（还有字段未记录）"; break; }
          o[k] = pick(v, k, depth + 1);
        }
        return o;
      }

      // 取值本身就可能抛（getter / Proxy）：读不到就照实写，绝不让插件的对象把探针带崩。
      function pick(obj, key, depth) {
        try {
          return brief(obj[key], depth, key);
        } catch (e) {
          return "«读取该字段抛异常»";
        }
      }

      // 摘要 → JSON 文本（管理中心会 JSON.parse 后美化显示；解析不了就原样展示）
      function text(v) {
        var s;
        try {
          s = JSON.stringify(brief(v, 0));
        } catch (e) {
          try { s = String(v); } catch (e2) { s = "«无法序列化»"; }
        }
        return clip(s === undefined ? "" : s);
      }

      function entry(kind, level, method, args, result, ms, ok) {
        // pluginId / seq / at 一律由后端盖章补齐，探针不填（也伪造不了）
        return { kind: kind, level: level, method: method, args: args, result: result, ms: ms, ok: ok };
      }

      // ---------- 上报（节流 + 批量） ----------
      function record(e) {
        try {
          if (queue.length >= QUEUE_MAX) { queue.shift(); dropped++; }
          queue.push(e);
          if (queue.length >= BATCH) flush();
          else if (timer === null) timer = setTimeout(flush, FLUSH_MS);
        } catch (err) { /* 记录失败就放弃这条：绝不影响插件 */ }
      }

      function after() {
        sending = false;
        if (queue.length && timer === null) timer = setTimeout(flush, 0);
      }

      function flush() {
        try {
          if (timer !== null) { clearTimeout(timer); timer = null; }
          if (!queue.length || sending) {
            // 上一批还在路上：等它回来再发（保持时序，也避免 IPC 洪水）
            if (queue.length && timer === null) timer = setTimeout(flush, FLUSH_MS);
            return;
          }
          var batch = queue.splice(0, queue.length);
          if (dropped) {
            batch.unshift(entry("console", "warn", "probe.overflow", "",
              "上报通道积压，已丢弃 " + dropped + " 条日志（那些调用确实发生过，只是没记下来）", 0, true));
            dropped = 0;
          }
          sending = true;
          // ⚠ 必须 rawInvoke：走门面 invoke 会被本探针记成新日志 → 再触发上报 → 无限自激。
          rawInvoke("dev_log_push", { entries: batch }).then(after, function (err) {
            // 上报失败就丢掉这批：重新排队会在后端持续拒绝时变成死循环。
            // 只用**原始** console.warn 提醒一次（它没被钩住，不会再生成日志）。
            if (!warnedPushFail) {
              warnedPushFail = true;
              if (origWarn) {
                try { origWarn("[iTools 调试探针] 日志上报失败，这些日志不会出现在开发者中心：", err); } catch (e) {}
              }
            }
            after();
          });
        } catch (err) {
          sending = false;
        }
      }

      // ---------- 门面 invoke 包装（本探针的核心注入点） ----------
      // 命令名 → itools.* 里的 API 名（日志面板里直接看得懂）。未登记的原样显示命令名，不编造。
      var API_NAME = {
        plugin_take_enter: "onEnter（取进入上下文）",
        plugin_hide: "hide", plugin_exit: "exit", plugin_set_height: "setHeight",
        plugin_register_hotkey: "registerHotkey", plugin_unregister_hotkey: "unregisterHotkey",
        plugin_copy_text: "copyText", plugin_read_text: "readText",
        plugin_read_image: "readImage", plugin_write_image: "writeImage", plugin_save_image: "saveImage",
        plugin_create_pin: "createPin", plugin_ocr: "ocr",
        plugin_start_audio_record: "startAudioRecord", plugin_stop_audio_record: "stopAudioRecord",
        plugin_start_gif_record: "startGifRecord", plugin_stop_gif_record: "stopGifRecord",
        plugin_read_file: "readFile", plugin_write_file: "writeFile", plugin_remove_file: "removeFile",
        plugin_read_local_image: "readLocalImage",
        plugin_list_displays: "listDisplays", plugin_capture_full: "captureFull",
        plugin_capture_region: "captureRegion",
        plugin_open_external: "openExternal", plugin_open_path: "openPath",
        plugin_notify: "notify", plugin_run_command: "runCommand", plugin_fetch: "fetch",
        plugin_db_get: "db.get", plugin_db_set: "db.set", plugin_db_remove: "db.remove", plugin_db_keys: "db.keys",
        plugin_account_state: "account.state",
        plugin_data_get: "data.get", plugin_data_set: "data.set", plugin_data_remove: "data.remove",
        plugin_data_keys: "data.keys", plugin_data_sync: "data.sync",
        plugin_get_setting: "settings.get", plugin_get_settings: "settings.all"
      };
      function apiName(cmd) {
        return Object.prototype.hasOwnProperty.call(API_NAME, cmd) ? API_NAME[cmd] : String(cmd);
      }
      function apiEntry(cmd, args, ms, ok, payload) {
        return entry("api", ok ? "info" : "error", apiName(cmd), text(args || {}), text(payload),
          Math.round(ms * 100) / 100, !!ok);
      }

      var pass = invoke; // 此刻就是 rawInvoke
      invoke = function (cmd, args) {
        // 双保险：上报命令永不被记录（flush 本就直接用 rawInvoke，这里挡的是将来的误改）
        if (cmd === "dev_log_push") return pass(cmd, args);
        var t0 = nowMs();
        var p;
        try {
          p = pass(cmd, args);
        } catch (e) {
          record(apiEntry(cmd, args, nowMs() - t0, false, e));
          throw e; // 同步异常原样透传
        }
        try {
          if (p && typeof p.then === "function") {
            // 只观察不改写：两个回调都给，派生 promise 不会产生「未处理拒绝」噪声；
            // 返回给插件的始终是原始 p。
            p.then(
              function (r) { record(apiEntry(cmd, args, nowMs() - t0, true, r)); },
              function (e) { record(apiEntry(cmd, args, nowMs() - t0, false, e)); }
            );
          }
        } catch (e) { /* 观察失败不影响调用本身 */ }
        return p;
      };

      // ---------- console 钩子（原始行为完整保留） ----------
      var LEVEL = { log: "info", info: "info", debug: "info", warn: "warn", error: "error" };
      function consoleArg(v) {
        try {
          if (typeof v === "string") return briefStr(v, false);
          if (v instanceof Error) return clip(String(v.stack || v));
          return text(v);
        } catch (e) {
          return "«无法记录的参数»";
        }
      }
      try {
        Object.keys(LEVEL).forEach(function (name) {
          var orig = window.console && window.console[name];
          if (typeof orig !== "function") return;
          if (name === "warn") {
            origWarn = function () { orig.apply(window.console, arguments); };
          }
          window.console[name] = function () {
            // 原始行为**必须**先执行且完整保留：DevTools 里照常看得到，
            // 即便下面的记录逻辑抛了，插件的日志也已经打出去了。
            try { orig.apply(window.console, arguments); } catch (e) {}
            try {
              var parts = [];
              for (var i = 0; i < arguments.length && i < MAX_ITEMS; i++) parts.push(consoleArg(arguments[i]));
              if (arguments.length > MAX_ITEMS) parts.push("…（还有 " + (arguments.length - MAX_ITEMS) + " 个参数）");
              record(entry("console", LEVEL[name], "console." + name, "",
                clip(parts.join(" ")), 0, LEVEL[name] !== "error"));
            } catch (e) {}
          };
        });
      } catch (e) {}

      // ---------- 未捕获异常 / 未处理的 Promise 拒绝 / 资源加载失败 ----------
      // 用 addEventListener 而不是覆盖 window.onerror：不抢占插件自己注册的处理器。
      // capture=true 才收得到资源加载错误（<img>/<script> 的 error 不冒泡）。
      window.addEventListener("error", function (ev) {
        try {
          var tgt = ev && ev.target;
          if (tgt && tgt !== window && tgt.tagName) {
            record(entry("error", "error", "resource.error", "",
              "资源加载失败：<" + String(tgt.tagName).toLowerCase() + "> " +
              briefStr(String(tgt.src || tgt.href || ""), false), 0, false));
            return;
          }
          var err = ev && ev.error;
          var where = ev && ev.filename ? "\n  at " + ev.filename + ":" + ev.lineno + ":" + ev.colno : "";
          record(entry("error", "error", "window.onerror", "",
            clip(String((ev && ev.message) || "未捕获异常") + where + (err && err.stack ? "\n" + err.stack : "")),
            0, false));
        } catch (e) {}
      }, true);

      window.addEventListener("unhandledrejection", function (ev) {
        try {
          var r = ev ? ev.reason : undefined;
          var body;
          if (r && r.stack) body = String(r.stack);
          else if (typeof r === "string") body = r;
          else if (r === undefined) body = "（reason 为空）";
          else body = text(r);
          record(entry("error", "error", "unhandledrejection", "", clip(body), 0, false));
        } catch (e) {}
      });

      // 页面卸载 / 切到后台：尽力把攒着的日志发出去。IPC 是异步的，来不及的那部分会随页面消失，
      // 这一点在管理中心面板的说明里已写明——不做「保证送达」的承诺。
      window.addEventListener("pagehide", flush);
      document.addEventListener("visibilitychange", function () {
        if (document.visibilityState === "hidden") flush();
      });

      // 后端推来的事件（热键触发、设置变更…）也记一条：否则「按了热键没反应」查不出
      // 是后端没推来，还是插件的回调抛了。
      probe = {
        event: function (channel, payload) {
          try {
            record(entry("api", "info", "event:" + String(channel), text(payload),
              "后端推送到插件页的事件（不是插件发起的调用）", 0, true));
          } catch (e) {}
        }
      };

      // 第一条日志：把探针的覆盖范围与盲区写进时间线本身，开发者一眼可见，不必翻文档。
      record(entry("console", "info", "probe.ready", "",
        "调试探针已挂载。覆盖：window.itools.* 的全部调用（入参 / 返回摘要 + 耗时 + 成功失败）、" +
        "console.log/info/debug/warn/error、未捕获异常与未处理的 Promise 拒绝、资源加载失败、后端推来的事件。" +
        "⚠ 盲区：插件若绕过门面直接调 window.__TAURI_INTERNALS__.invoke()，探针抓不到" +
        "（Tauri 把它定义成不可写不可配置，无法挂钩）；页面卸载瞬间未及上报的日志也会丢。" +
        "所以「日志里没有」不等于「没发生」。", 0, true));
    } catch (e) {
      // 探针装不上就当没有：绝不影响插件本体运行。
      probe = null;
      try {
        if (window.console && typeof window.console.warn === "function") {
          window.console.warn("[iTools 调试探针] 挂载失败，本次调试不会产生 API / console 日志：", e);
        }
      } catch (e2) {}
    }
  }
})();
