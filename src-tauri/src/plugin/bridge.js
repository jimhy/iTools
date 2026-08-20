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

  // ==================== 插件间数据隔离：清空浏览器级存储 ====================
  //
  // 所有插件页**同源**：URL 是 `itplugin://localhost/<id>/<path>`（Windows 上表现为
  // `http://itplugin.localhost/...`），插件 id 在**路径段**而不在 host。于是
  // localStorage / sessionStorage / IndexedDB / Cache Storage 这些浏览器级存储
  // 在所有插件之间是**共享**的——插件 B 一行 `localStorage.getItem(...)` 就能读到
  // 插件 A 存的东西，还能改、能删。后端的 db / data / settings / 沙盒文件都按插件
  // 隔离得很干净，唯独这一面是敞开的。
  //
  // 本文件是 initialization_script，**在插件页任何脚本之前执行**。因此「每次进入插件
  // 先清空」就能保证：任何插件启动时看到的浏览器级存储都是空的，读不到上一个插件的
  // 任何残留。跨插件读取这条路由此堵死。
  //
  // 代价（已写进插件开发规范，必须让作者知道）：浏览器级存储对插件而言是**会话级**的，
  // 退出插件即失效。插件要持久化必须用 itools.db / itools.data（后端按插件 id 隔离）。
  //
  // 为什么不改成「每插件独立 origin」（`<id>.itplugin.localhost`）：那是更彻底的解法，
  // 浏览器会天然隔离，但它依赖 Tauri 与 WebView2 对自定义协议**子域名**的处理方式，
  // 未经实测验证；本方案不依赖任何平台行为，先把洞堵上。独立 origin 作为后续根本解。
  //
  // 注意别把清理写成「异步做完再放行」：IndexedDB / Cache 的清理是异步的，等它们会拖慢
  // 每次进插件的速度。这里同步清掉最要紧的 localStorage / sessionStorage（跨插件读取
  // 几乎都走这两个），异步的那两类发起后不等待——它们要被读到得先 await，而那时删除
  // 请求早已排在前面。
  function purgeWebStorage() {
    try {
      window.localStorage && window.localStorage.clear();
    } catch (_) {}
    try {
      window.sessionStorage && window.sessionStorage.clear();
    } catch (_) {}
    try {
      if (window.indexedDB && typeof window.indexedDB.databases === "function") {
        window.indexedDB.databases().then(function (list) {
          (list || []).forEach(function (d) {
            try {
              d && d.name && window.indexedDB.deleteDatabase(d.name);
            } catch (_) {}
          });
        }, function () {});
      }
    } catch (_) {}
    try {
      if (window.caches && typeof window.caches.keys === "function") {
        window.caches.keys().then(function (keys) {
          (keys || []).forEach(function (k) {
            try {
              window.caches.delete(k);
            } catch (_) {}
          });
        }, function () {});
      }
    } catch (_) {}
  }
  purgeWebStorage();

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

  // 后台常驻插件被再次唤起：页面一直活着，不会重新加载，所以拿不到新的 plugin_take_enter。
  // 由 Rust 侧推一发 'enter' 事件让 onEnter 重跑一遍——这样插件既能收到本次的
  // code/type/query，又保住了它在后台攒下的内存状态（监听器、缓存），不像 reload 那样清零。
  onChannel("enter", function (p) {
    if (!p) return;
    fireEnter({ code: p.code, type: p.type, query: p.query, files: p.files || [] });
  });

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
        fireEnter({ code: p.code, type: p.type, query: p.query, files: p.files || [] });
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
        // "binary" 时返回体是 base64（结果对象的 base64 字段为 true）。缺省按文本返回，
        // 老插件的行为逐字节不变。
        responseType: init.responseType || null,
      });
    },
    // 下载文件到插件沙盒，带进度。进度经 "download-progress" 事件回报：
    // { id, received, total, done, error }；total 为 null 表示服务端没给 Content-Length。
    // id 由调用方生成（下载开始后才拿到 id 的话就没法中途取消了）。
    download: function (url, dest, id, onProgress) {
      var did = String(id);
      if (typeof onProgress === "function") {
        onChannel("download-progress", function (p) {
          if (p && p.id === did) onProgress(p);
        });
      }
      return invoke("plugin_download", { url: String(url), dest: String(dest), id: did });
    },
    downloadCancel: function (id) {
      return invoke("plugin_download_cancel", { id: String(id) });
    },
    // 执行外部程序，拿得到 stdout/退出码（需 runCommand 授权）。
    // 注意 truncated：为 true 说明输出超过 16MB 上限被截断，此时不能据 stdout 下
    // 「没有结果」这类结论——要完整输出请用 execStream。
    exec: function (program, args, opts) {
      return invoke("plugin_exec", {
        program: String(program),
        args: args || [],
        opts: opts || null,
      });
    },
    // 流式执行：边跑边收输出。返回 streamId，用它 execKill/execQuit。
    // handlers: { onData, onErr, onExit }
    execStream: function (program, args, opts, handlers) {
      handlers = handlers || {};
      return invoke("plugin_exec_stream", {
        program: String(program),
        args: args || [],
        opts: opts || null,
      }).then(function (streamId) {
        var mine = function (p) {
          return p && p.streamId === streamId;
        };
        if (handlers.onData) {
          onChannel("plugin-exec-stdout", function (p) {
            if (mine(p)) handlers.onData(p.data);
          });
        }
        if (handlers.onErr) {
          onChannel("plugin-exec-stderr", function (p) {
            if (mine(p)) handlers.onErr(p.data);
          });
        }
        if (handlers.onExit) {
          onChannel("plugin-exec-exit", function (p) {
            if (mine(p)) handlers.onExit(p.code, p.timedOut);
          });
        }
        return streamId;
      });
    },
    execKill: function (streamId) {
      return invoke("plugin_exec_kill", { streamId: String(streamId) });
    },
    execQuit: function (streamId) {
      return invoke("plugin_exec_quit", { streamId: String(streamId) });
    },
    // 用户选择即授权的文件访问：用户亲自选一次目录/文件，插件拿到该范围的持久句柄。
    // 句柄跨会话保留（listScopes 可枚举），插件永远只能碰用户点过头的范围。
    fs: {
      pickDir: function (opts) {
        return invoke("plugin_pick_dir", { opts: opts || null });
      },
      pickFile: function (opts) {
        return invoke("plugin_pick_file", { opts: opts || null });
      },
      listScopes: function () {
        return invoke("plugin_fs_list_scopes");
      },
      revokeScope: function (scopeId) {
        return invoke("plugin_fs_revoke_scope", { scopeId: String(scopeId) });
      },
      list: function (scopeId, subPath) {
        return invoke("plugin_fs_list", {
          scopeId: String(scopeId),
          subPath: subPath != null ? String(subPath) : null,
        });
      },
      stat: function (scopeId, path) {
        return invoke("plugin_fs_stat", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
        });
      },
      hash: function (scopeId, path, algo) {
        return invoke("plugin_fs_hash", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
          algo: String(algo || "sha256"),
        });
      },
      // read/write 走 base64：用户的真实文件可能是任意二进制，按 UTF-8 文本读会损坏内容。
      read: function (scopeId, path) {
        return invoke("plugin_fs_read", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
        });
      },
      readChunk: function (scopeId, path, offset, len) {
        return invoke("plugin_fs_read_chunk", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
          offset: Number(offset) || 0,
          len: Number(len) || 0,
        });
      },
      write: function (scopeId, path, contentB64) {
        return invoke("plugin_fs_write", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
          contentB64: String(contentB64),
        });
      },
      // 压缩 / 解压（限 scope 内）。解压做了 Zip Slip 防护与条目数/体积上限。
      zipCreate: function (scopeId, entries, outPath) {
        return invoke("plugin_zip_create", {
          scopeId: String(scopeId),
          entries: entries || [],
          outPath: String(outPath),
        });
      },
      unzip: function (scopeId, zipPath, outSub) {
        return invoke("plugin_unzip", {
          scopeId: String(scopeId),
          zipPath: String(zipPath),
          outSub: outSub != null ? String(outSub) : null,
        });
      },
      // 目录变化监听（ReadDirectoryChangesW 事件驱动，非轮询；同一路径 300ms 内的多次写入已合并）
      // 事件：{ watchId, kind: created|modified|removed|renamed|error, path, oldPath?, message? }
      watchStart: function (scopeId, subPath, cb) {
        if (typeof cb === "function") onChannel("plugin-fs-watch", cb);
        return invoke("plugin_fs_watch_start", {
          scopeId: String(scopeId),
          subPath: subPath != null ? String(subPath) : null,
        });
      },
      watchStop: function (watchId) {
        return invoke("plugin_fs_watch_stop", { watchId: String(watchId) });
      },
      // 系统文件图标（base64 PNG）
      getFileIcon: function (scopeId, path) {
        return invoke("plugin_get_file_icon", {
          scopeId: String(scopeId),
          path: path != null ? String(path) : null,
        });
      },
    },
    // 命名系统位置（白名单，不接受任意路径）与回收站。
    paths: {
      resolve: function (name) {
        return invoke("plugin_paths_resolve", { name: String(name) });
      },
      scan: function (name, opts) {
        return invoke("plugin_paths_scan", { name: String(name), opts: opts || null });
      },
    },
    // 送回收站（可还原）。iTools 不给插件「真删」——真删由宿主确认后执行。
    trash: function (paths) {
      return invoke("plugin_trash", { paths: paths || [] });
    },
    // 图像处理（纯计算，无需授权）
    image: {
      resize: function (data, width, height, mode) {
        return invoke("plugin_image_resize", {
          data: String(data),
          width: Number(width),
          height: Number(height),
          mode: mode || "contain",
        });
      },
      crop: function (data, x, y, w, h) {
        return invoke("plugin_image_crop", {
          data: String(data),
          x: Number(x),
          y: Number(y),
          w: Number(w),
          h: Number(h),
        });
      },
      convert: function (data, format) {
        return invoke("plugin_image_convert", { data: String(data), format: String(format) });
      },
      compress: function (data, quality) {
        return invoke("plugin_image_compress", { data: String(data), quality: Number(quality) });
      },
      info: function (data) {
        return invoke("plugin_image_info", { data: String(data) });
      },
    },
    // 屏幕辅助：鼠标位置、取色、DIP↔物理像素换算（多屏高 DPI 下不换算会错位）
    screen: {
      cursorPoint: function () {
        return invoke("plugin_screen_cursor_point");
      },
      pickColorAt: function (x, y) {
        return invoke("plugin_screen_pick_color_at", { x: Number(x), y: Number(y) });
      },
      toDip: function (x, y) {
        return invoke("plugin_screen_to_dip", { x: Number(x), y: Number(y) });
      },
      toPhysical: function (x, y) {
        return invoke("plugin_screen_to_physical", { x: Number(x), y: Number(y) });
      },
      rectToDip: function (x, y, width, height) {
        return invoke("plugin_screen_rect_to_dip", {
          x: Number(x),
          y: Number(y),
          width: Number(width),
          height: Number(height),
        });
      },
      rectToPhysical: function (x, y, width, height) {
        return invoke("plugin_screen_rect_to_physical", {
          x: Number(x),
          y: Number(y),
          width: Number(width),
          height: Number(height),
        });
      },
    },
    // 系统信息与标准路径。getPath 只返回路径字符串，不等于获得访问权限——
    // 要访问用户文件仍须走 itools.fs 的 scope 授权。
    sys: {
      info: function () {
        return invoke("plugin_sys_info");
      },
      usage: function () {
        return invoke("plugin_sys_usage");
      },
      getPath: function (name) {
        return invoke("plugin_sys_get_path", { name: String(name) });
      },
    },
    showItemInFolder: function (path) {
      return invoke("plugin_show_item_in_folder", { path: String(path) });
    },
    // SQLite（按插件隔离的本地库）。数据量大、要做关联查询时用它，KV 存不下的场景。
    // params 是数组，值支持 null/bool/number/string；BLOB 用 { $blob: base64 } 标记。
    // 注意 ATTACH / DETACH / 危险 PRAGMA 会被拒绝——那是插件间数据隔离的红线。
    sqlite: {
      open: function (name) {
        return invoke("plugin_sqlite_open", { name: String(name) });
      },
      exec: function (handle, sql, params) {
        return invoke("plugin_sqlite_exec", {
          handle: String(handle),
          sql: String(sql),
          params: params || null,
        });
      },
      query: function (handle, sql, params) {
        return invoke("plugin_sqlite_query", {
          handle: String(handle),
          sql: String(sql),
          params: params || null,
        });
      },
      batch: function (handle, statements) {
        return invoke("plugin_sqlite_batch", {
          handle: String(handle),
          statements: statements || [],
        });
      },
      close: function (handle) {
        return invoke("plugin_sqlite_close", { handle: String(handle) });
      },
    },
    // 输入注入（需 input-inject 授权）。
    // ⚠️ 注入前请先 itools.hide()，否则输入会打到 iTools 自己身上。
    // ⚠️ 受 Windows UIPI 限制：无法向以管理员权限运行的窗口注入（表现为「时灵时不灵」）。
    input: {
      // 按输入法原理输入任意字符串，支持 Emoji / 中文，不是逐键模拟
      typeString: function (text) {
        return invoke("plugin_input_type_string", { text: String(text) });
      },
      pasteText: function (text) {
        return invoke("plugin_input_paste_text", { text: String(text) });
      },
      pasteFile: function (paths) {
        return invoke("plugin_input_paste_file", { paths: paths || [] });
      },
      pasteImage: function (data) {
        return invoke("plugin_input_paste_image", { data: toImgB64(data) });
      },
      keyTap: function (key, modifiers) {
        return invoke("plugin_input_key_tap", { key: String(key), modifiers: modifiers || [] });
      },
      mouseMove: function (x, y) {
        return invoke("plugin_input_mouse_move", { x: Number(x), y: Number(y) });
      },
      mouseClick: function (x, y) {
        return invoke("plugin_input_mouse_click", { x: Number(x), y: Number(y) });
      },
      mouseDoubleClick: function (x, y) {
        return invoke("plugin_input_mouse_double_click", { x: Number(x), y: Number(y) });
      },
      mouseRightClick: function (x, y) {
        return invoke("plugin_input_mouse_right_click", { x: Number(x), y: Number(y) });
      },
    },
    // 剪贴板变化监听（需 clipboard-watch 授权）。事件只带序列号，内容自行再读——
    // 免得每次变化都把剪贴板全文推一遍。
    clipboard: {
      watchStart: function () {
        return invoke("plugin_clipboard_watch_start");
      },
      watchStop: function () {
        return invoke("plugin_clipboard_watch_stop");
      },
      onChange: function (cb) {
        onChannel("plugin-clipboard-changed", cb);
      },
    },
    // 桌面窗口管理（需 window-manage 授权）。操作的是**别的应用**的窗口；
    // iTools 自己的窗口一律拒绝，避免插件把宿主搞成不可恢复状态。
    win: {
      list: function () {
        return invoke("plugin_win_list");
      },
      getForeground: function () {
        return invoke("plugin_win_get_foreground");
      },
      // 返回 { success, reason }：Windows 的前台抢占限制会让激活静默失败，
      // 这里如实回报成败，别当成一定成功
      focus: function (hwnd) {
        return invoke("plugin_win_focus", { hwnd: Number(hwnd) });
      },
      move: function (hwnd, x, y) {
        return invoke("plugin_win_move", { hwnd: Number(hwnd), x: Number(x), y: Number(y) });
      },
      resize: function (hwnd, w, h) {
        return invoke("plugin_win_resize", { hwnd: Number(hwnd), w: Number(w), h: Number(h) });
      },
      setRect: function (hwnd, rect) {
        return invoke("plugin_win_set_rect", { hwnd: Number(hwnd), rect: rect });
      },
      minimize: function (hwnd) {
        return invoke("plugin_win_minimize", { hwnd: Number(hwnd) });
      },
      maximize: function (hwnd) {
        return invoke("plugin_win_maximize", { hwnd: Number(hwnd) });
      },
      restore: function (hwnd) {
        return invoke("plugin_win_restore", { hwnd: Number(hwnd) });
      },
      // 只保证 WM_CLOSE 投递成功，不保证目标真的关闭（它可能弹保存对话框或忽略）
      close: function (hwnd) {
        return invoke("plugin_win_close", { hwnd: Number(hwnd) });
      },
      setTopmost: function (hwnd, on) {
        return invoke("plugin_win_set_topmost", { hwnd: Number(hwnd), on: !!on });
      },
    },
    // 托管式本地 HTTP 服务（需 local-server 授权）：把用户已授权的目录暴露到局域网，
    // 手机扫码即可访问。端口由宿主开、宿主关；默认必带访问令牌。
    // 二维码请插件自己用内联 JS 生成——宿主只返回 urls。
    serve: {
      start: function (opts) {
        return invoke("plugin_serve_start", { opts: opts || {} });
      },
      stop: function (serveId) {
        return invoke("plugin_serve_stop", { serveId: String(serveId) });
      },
      list: function () {
        return invoke("plugin_serve_list");
      },
    },
    lan: {
      announce: function (opts) {
        return invoke("plugin_lan_announce", { opts: opts || {} });
      },
      discover: function (timeoutMs) {
        return invoke("plugin_lan_discover", { timeoutMs: Number(timeoutMs) || 1500 });
      },
    },
    // 进程 / 已装软件 / 启动项 / 电源（分别需 process-manage、system-read、system-manage 授权）
    proc: {
      list: function () {
        return invoke("plugin_proc_list");
      },
      kill: function (pid) {
        return invoke("plugin_proc_kill", { pid: Number(pid) });
      },
    },
    installedApps: function () {
      return invoke("plugin_installed_apps");
    },
    startup: {
      list: function () {
        return invoke("plugin_startup_list");
      },
      remove: function (id) {
        return invoke("plugin_startup_remove", { id: String(id) });
      },
      setEnabled: function (id, on) {
        return invoke("plugin_startup_set_enabled", { id: String(id), on: !!on });
      },
    },
    power: {
      lock: function () {
        return invoke("plugin_power_lock");
      },
      sleep: function () {
        return invoke("plugin_power_sleep");
      },
      shutdown: function (force) {
        return invoke("plugin_power_shutdown", { force: !!force });
      },
      restart: function (force) {
        return invoke("plugin_power_restart", { force: !!force });
      },
    },
    // 宿主托管的外部运行时（ffmpeg / adb / yt-dlp，需 runtime 授权）。
    // 插件包禁止自带二进制，这些程序由宿主按官方清单下载并校验哈希后统一管理，
    // 插件拿不到真实路径，只能用 name + args 调用。
    runtime: {
      list: function () {
        return invoke("plugin_runtime_list");
      },
      // 未安装则下载安装；onProgress 收 { name, received, total, done, error }
      ensure: function (name, onProgress) {
        if (typeof onProgress === "function") {
          onChannel("plugin-runtime-progress", function (p) {
            if (p && p.name === name) onProgress(p);
          });
        }
        return invoke("plugin_runtime_ensure", { name: String(name) });
      },
      exec: function (name, args, opts) {
        return invoke("plugin_runtime_exec", {
          name: String(name),
          args: args || [],
          opts: opts || null,
        });
      },
      // handlers: { onStdout, onStderr, onExit, onFfmpegProgress }
      // onFfmpegProgress 只在 name === "ffmpeg" 时有：宿主已把 ffmpeg 的进度行解析成
      // { frame, fps, bitrate, time, timeSecs, speed }，不必每个插件自己写解析器
      execStream: function (name, args, opts, handlers) {
        handlers = handlers || {};
        return invoke("plugin_runtime_exec_stream", {
          name: String(name),
          args: args || [],
          opts: opts || null,
        }).then(function (streamId) {
          var mine = function (p) {
            return p && p.streamId === streamId;
          };
          if (handlers.onStdout) {
            onChannel("plugin-runtime-stdout", function (p) {
              if (mine(p)) handlers.onStdout(p.data);
            });
          }
          if (handlers.onStderr) {
            onChannel("plugin-runtime-stderr", function (p) {
              if (mine(p)) handlers.onStderr(p.data);
            });
          }
          if (handlers.onExit) {
            onChannel("plugin-runtime-exit", function (p) {
              if (mine(p)) handlers.onExit(p.code, p.timedOut);
            });
          }
          if (handlers.onFfmpegProgress) {
            onChannel("plugin-runtime-ffmpeg-progress", function (p) {
              if (mine(p)) handlers.onFfmpegProgress(p);
            });
          }
          return streamId;
        });
      },
      kill: function (streamId) {
        return invoke("plugin_runtime_exec_kill", { streamId: String(streamId) });
      },
      quit: function (streamId) {
        return invoke("plugin_runtime_exec_quit", { streamId: String(streamId) });
      },
      remove: function (name) {
        return invoke("plugin_runtime_remove", { name: String(name) });
      },
    },
    // 摄像头（需 camera 授权）。⚠️ 使用中托盘会显示「XX 正在使用摄像头」，用户可一键掐断——
    // 这是隐私底线，插件无法隐藏它。
    // ⚠️ 预览流每帧都要过一次 IPC（base64 JPEG），分辨率和帧率别开太高。
    camera: {
      list: function () {
        return invoke("plugin_camera_list");
      },
      grab: function (deviceId, opts) {
        return invoke("plugin_camera_grab", { deviceId: String(deviceId), opts: opts || null });
      },
      // onFrame 收 { streamId, b64, width, height }（b64 是 JPEG，无 data URI 前缀）
      // onStopped 只在**意外**结束时触发（设备被拔、被别的程序抢走），主动 stop 不触发
      streamStart: function (deviceId, opts, handlers) {
        handlers = handlers || {};
        return invoke("plugin_camera_stream_start", {
          deviceId: String(deviceId),
          opts: opts || null,
        }).then(function (streamId) {
          var mine = function (p) { return p && p.streamId === streamId; };
          if (handlers.onFrame) {
            onChannel("plugin-camera-frame", function (p) { if (mine(p)) handlers.onFrame(p); });
          }
          if (handlers.onStopped) {
            onChannel("plugin-camera-stream-stopped", function (p) {
              if (mine(p)) handlers.onStopped(p.reason);
            });
          }
          return streamId;
        });
      },
      streamStop: function (streamId) {
        return invoke("plugin_camera_stream_stop", { streamId: String(streamId) });
      },
    },
    // 系统内录（录电脑正在播放的声音）与 mp4 录屏。
    // ⚠️ mp4 录屏依赖宿主托管的 ffmpeg：先 itools.runtime.ensure("ffmpeg")，否则明确报错。
    // ⚠️ 使用中托盘同样会显示并可掐断。
    record: {
      loopbackStart: function () {
        return invoke("plugin_audio_loopback_start");
      },
      loopbackStop: function () {
        return invoke("plugin_audio_loopback_stop");
      },
      // opts: { area?, displayId?, hwnd?, fps?, includeSystemAudio?, includeMic? }
      // onProgress 收 { recordId, elapsedMs, frames, sizeBytes }，约 500ms 一次
      videoStart: function (opts, onProgress) {
        if (typeof onProgress === "function") {
          onChannel("plugin-record-video-progress", onProgress);
        }
        return invoke("plugin_record_video_start", { opts: opts || {} });
      },
      videoStop: function (recordId) {
        return invoke("plugin_record_video_stop", { recordId: String(recordId) });
      },
    },
    // 加密存储：与 db 一样按插件隔离，区别是值加密落盘（Windows DPAPI，绑定当前用户账户）。
    // ⚠️ 防的是「别的用户账户 / 拷走磁盘文件的人」，**防不了**在当前用户下运行的其它程序。
    crypto: {
      set: function (key, value) {
        return invoke("plugin_crypto_set", { key: String(key), value: JSON.stringify(value) });
      },
      get: function (key) {
        return invoke("plugin_crypto_get", { key: String(key) }).then(function (v) {
          if (v == null) return null;
          try {
            return JSON.parse(v);
          } catch (_) {
            return v;
          }
        });
      },
      remove: function (key) {
        return invoke("plugin_crypto_remove", { key: String(key) });
      },
      keys: function (prefix) {
        return invoke("plugin_crypto_keys", { prefix: prefix != null ? String(prefix) : null });
      },
    },
    // 附件存储：存二进制大对象（图片 / 音频 / 导出文件），按插件隔离，单个上限 32MB
    attach: {
      put: function (id, data, mime) {
        return invoke("plugin_attach_put", {
          id: String(id),
          dataB64: toImgB64(data),
          mime: String(mime || "application/octet-stream"),
        });
      },
      get: function (id) {
        return invoke("plugin_attach_get", { id: String(id) });
      },
      remove: function (id) {
        return invoke("plugin_attach_remove", { id: String(id) });
      },
      list: function () {
        return invoke("plugin_attach_list");
      },
    },
    // 定时任务（需 background 授权）：到点推 plugin-schedule-fire 事件，不会自作主张弹窗口。
    // 只支持固定间隔 everySecs，不支持 cron 表达式。
    schedule: {
      add: function (opts) {
        return invoke("plugin_schedule_add", { opts: opts || {} });
      },
      remove: function (taskId) {
        return invoke("plugin_schedule_remove", { taskId: String(taskId) });
      },
      list: function () {
        return invoke("plugin_schedule_list");
      },
      onFire: function (cb) {
        onChannel("plugin-schedule-fire", cb);
      },
    },
    // 搜索结果注入：让本插件的结果直接出现在**主搜索框**里（用户边打字就能看到）。
    //
    // 前提：plugin.json 里至少一个 feature 声明 "mainPush": true。这类插件会被自动后台常驻，
    // 不需要用户去开自启动开关。
    //
    // ⚠️ 与 registerTool 一样，必须在页面初始化时调（与 onEnter 同级），别写在 onEnter 里。
    // ⚠️ 宿主只等 250ms：主搜索是逐键触发的，任何插件卡一下整个搜索框都会顿。
    //    回调里别做慢活（别发网络请求、别扫磁盘），超时的结果会被直接丢掉。
    // getList(query) 返回数组（或 Promise），每项：
    //   { title, subtitle?, payload?, code?, icon? }
    //   —— 用户选中后会打开本插件，payload 经 onEnter 的 info.query 送达。
    onMainPush: function (getList) {
      onChannel("main-push", function (p) {
        if (!p) return;
        var reply = function (items) {
          invoke("plugin_main_push_result", {
            roundId: p.roundId,
            items: Array.isArray(items) ? items : [],
          });
        };
        try {
          var r = getList(p.query || "");
          if (r && typeof r.then === "function") {
            r.then(reply, function () { reply([]); });
          } else {
            reply(r);
          }
        } catch (e) {
          console.error("[iTools] onMainPush 回调异常", e);
          reply([]);
        }
      });
      return invoke("plugin_register_main_push");
    },
    // 系统通知。点击通知：给了 featureCode 就唤起本插件的该功能，否则推 notify-click 事件。
    // actions 是动作按钮 [{ id, label }]，点击推 notify-action 事件（带 actionId）。
    notifyShow: function (opts) {
      return invoke("plugin_notify_show", { opts: opts || {} });
    },
    onNotifyClick: function (cb) {
      onChannel("plugin-notify-click", cb);
    },
    onNotifyAction: function (cb) {
      onChannel("plugin-notify-action", cb);
    },
    // 插件自己的托盘图标（需 tray 授权）。与 iTools 宿主的托盘完全隔离，改不到宿主那个。
    // 一个插件最多一个图标，重复 set 是更新。后台常驻类插件通常靠它作为唯一可见入口。
    tray: {
      set: function (opts) {
        return invoke("plugin_tray_set", { opts: opts || {} });
      },
      remove: function () {
        return invoke("plugin_tray_remove");
      },
      onClick: function (cb) {
        onChannel("plugin-tray-click", cb);
      },
      onMenu: function (cb) {
        onChannel("plugin-tray-menu", cb);
      },
    },
    // 把本插件的能力注册成 MCP 工具，供 Claude Code / Cursor 等**外部 AI** 直接调用。
    //
    // ⚠️ 必须在页面初始化时就调（与 onEnter 同级），**不要写在 onEnter 回调里**：
    // 外部 AI 调用时插件是被后台拉起的，写在某个触发分支里就不会执行，
    // AI 永远拿到「工具未注册」。
    //
    // name 必须与 plugin.json 的 tools 里的键一致。handler 收 (params, ctx)，
    // 返回值会回传给 AI；抛错则把错误信息回传。
    registerTool: function (name, handler) {
      onChannel("tool-call", function (p) {
        if (!p || p.name !== name) return;
        var done = false;
        var reply = function (result, error) {
          if (done) return;
          done = true;
          invoke("plugin_tool_result", {
            requestId: p.requestId,
            result: result != null ? (typeof result === "string" ? result : JSON.stringify(result)) : null,
            error: error != null ? String(error) : null,
          });
        };
        try {
          var r = handler(p.params || {}, { requestId: p.requestId });
          if (r && typeof r.then === "function") {
            r.then(function (v) { reply(v, null); }, function (e) { reply(null, e && e.message ? e.message : e); });
          } else {
            reply(r, null);
          }
        } catch (e) {
          reply(null, e && e.message ? e.message : e);
        }
      });
      return invoke("plugin_register_tool", { name: String(name) });
    },
    // 跳到另一个插件（label 可用「插件id#code」或对方的关键字/名字），payload 经对方的
    // onEnter info.query 送达。调用方会先隐去自己。
    redirect: function (label, payload) {
      return invoke("plugin_redirect", {
        label: String(label),
        payload: payload != null ? String(payload) : null,
      });
    },
    // 开一个独立窗口（可留在桌面上，与主面板并存）。page 是插件目录内的相对页面路径。
    createWindow: function (page, opts) {
      return invoke("plugin_create_window", { page: String(page), opts: opts || null });
    },
    closeWindow: function (label) {
      return invoke("plugin_close_window", { label: String(label) });
    },
    // 动态指令：运行时增删本插件的触发条目，不必改 plugin.json。
    // 加完立刻能在主搜索里搜到，与清单里的静态 feature 走同一套匹配规则。
    setFeature: function (feature) {
      return invoke("plugin_set_feature", { feature: feature });
    },
    removeFeature: function (code) {
      return invoke("plugin_remove_feature", { code: String(code) });
    },
    getFeatures: function (codes) {
      return invoke("plugin_get_features", { codes: codes || null });
    },
    // 上下文感知：用户唤起 iTools 之前，前台是哪个窗口。
    // 拿不到就返回 null（比如浏览器不是 Chrome/Edge、或页面还没加载出地址），
    // 不会编一个看起来像样的值糊弄调用方——判空是调用方必须做的。
    context: {
      activeWindow: function () {
        return invoke("plugin_context_active_window");
      },
      browserUrl: function () {
        return invoke("plugin_context_browser_url");
      },
      folderPath: function () {
        return invoke("plugin_context_folder_path");
      },
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
        plugin_get_setting: "settings.get", plugin_get_settings: "settings.all",
        // ↓ 能力开放第一波新增
        plugin_download: "download", plugin_download_cancel: "downloadCancel",
        plugin_exec: "exec", plugin_exec_stream: "execStream",
        plugin_exec_kill: "execKill", plugin_exec_quit: "execQuit",
        plugin_pick_dir: "fs.pickDir", plugin_pick_file: "fs.pickFile",
        plugin_fs_list_scopes: "fs.listScopes", plugin_fs_revoke_scope: "fs.revokeScope",
        plugin_fs_list: "fs.list", plugin_fs_stat: "fs.stat", plugin_fs_hash: "fs.hash",
        plugin_fs_read: "fs.read", plugin_fs_read_chunk: "fs.readChunk", plugin_fs_write: "fs.write",
        plugin_paths_resolve: "paths.resolve", plugin_paths_scan: "paths.scan", plugin_trash: "trash",
        plugin_image_resize: "image.resize", plugin_image_crop: "image.crop",
        plugin_image_convert: "image.convert", plugin_image_compress: "image.compress",
        plugin_image_info: "image.info",
        plugin_screen_cursor_point: "screen.cursorPoint", plugin_screen_pick_color_at: "screen.pickColorAt",
        plugin_screen_to_dip: "screen.toDip", plugin_screen_to_physical: "screen.toPhysical",
        plugin_screen_rect_to_dip: "screen.rectToDip", plugin_screen_rect_to_physical: "screen.rectToPhysical",
        plugin_sys_info: "sys.info", plugin_sys_usage: "sys.usage", plugin_sys_get_path: "sys.getPath",
        plugin_show_item_in_folder: "showItemInFolder",
        plugin_context_active_window: "context.activeWindow",
        plugin_context_browser_url: "context.browserUrl",
        plugin_context_folder_path: "context.folderPath",
        plugin_sqlite_open: "sqlite.open", plugin_sqlite_exec: "sqlite.exec",
        plugin_sqlite_query: "sqlite.query", plugin_sqlite_batch: "sqlite.batch",
        plugin_sqlite_close: "sqlite.close"
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
