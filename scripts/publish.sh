#!/bin/sh
# iTools 发布到**自建服务线**：打包带服务端地址的安装包 + 发布官网静态页到群晖 NAS。
#
# 这条线与 GitHub Release 是**两条不同的分发线，产物不同**（别混）：
#
# - **GitHub Release（开源版）**：推 `v*` tag 由 .github/workflows/release.yml 构建，
#   刻意**不注入** ITOOLS_DEFAULT_ENDPOINT——下载者拿到的是「云端未接入」的干净客户端。
# - **官网下载（自建服务版）**：就是本脚本，构建时注入 ITOOLS_DEFAULT_ENDPOINT，
#   产物传到 NAS 由 itools-sync-server 顺带托管分发。
#
# # 用法
#
#   ./scripts/publish.sh site    # 只发官网静态页（秒级，改文案/样式后常用）
#   ./scripts/publish.sh app     # 只打包并上传安装包（要重编译，几分钟）
#   ./scripts/publish.sh all     # 两样都做（默认）
#
# # 配置
#
# 全部从仓库根的 `.publish.env` 读，**该文件在 .gitignore 里、不进仓库**
# ——这个仓库是公开的，运维拓扑不进源码（与 release.yml 同一条原则）。
# 首次使用照着 .publish.env.example 复制一份填好即可。
#
# # 这里每一条防呆都对应一次踩过的坑，别删
#
# 1. `option_env!` 在**编译期**求值，而 build.rs 没有 rerun-if-env-changed
#    → 改了 ITOOLS_DEFAULT_ENDPOINT 不 clean 的话，编出来的还是上次那个地址，
#      而且外观完全正常。所以 app 流程**强制先 cargo clean -p itools**。
# 2. 打完必须**搜二进制确认地址真的在里面**——不然发出去的包连不上服务器，
#    用户侧表现是「云端未接入」，回头才发现白发一版。
# 3. 群晖默认没开 SFTP 子系统，scp 直连报 subsystem request failed
#    → 一律用 `scp -O`（legacy 协议）。
# 4. **判断成败的命令绝不套管道**：管道会把退出码顶成末端进程的，
#    曾经因此把「构建失败」当成功交付过。要截输出就先重定向到文件再读。
# 5. 传完必须**核产物**（版本号端点 / 官网实际内容），不能只看 scp 退出码。

set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODE="${1:-all}"

# ---------- 读配置 ----------
ENV_FILE="$ROOT/.publish.env"
if [ ! -f "$ENV_FILE" ]; then
  echo "✗ 缺少配置文件 $ENV_FILE" >&2
  echo "  照着 .publish.env.example 复制一份并填好（该文件不进仓库）：" >&2
  echo "    cp .publish.env.example .publish.env" >&2
  exit 1
fi
# shellcheck disable=SC1090
. "$ENV_FILE"

: "${NAS_HOST:?.publish.env 里缺 NAS_HOST（形如 user@host）}"
NAS_PORT="${NAS_PORT:-22}"
NAS_SITE_DIR="${NAS_SITE_DIR:-/volume1/docker/itools-data/site}"
NAS_DOWNLOADS_DIR="${NAS_DOWNLOADS_DIR:-/volume1/docker/itools-data/downloads}"
# 核产物时用来实测的公开地址；留空则跳过线上核验那几步
SITE_URL="${SITE_URL:-}"

# 用 sed 而不是 node 读版本号：本脚本在 Git Bash 下跑，$ROOT 是 POSIX 路径
# （/f/...），而 Windows 版 node 不认它，require 会直接 MODULE_NOT_FOUND。
# package.json 顶层的 "version" 在最前面，取第一个匹配即可。
VERSION=$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$ROOT/package.json" | head -1)
[ -n "$VERSION" ] || { echo "✗ 读不出 package.json 里的版本号" >&2; exit 1; }

echo "==> iTools 发布（自建服务线）"
echo "    版本      : $VERSION"
echo "    目标      : $NAS_HOST:$NAS_PORT"
echo "    模式      : $MODE"
echo ""

# ---------- 官网静态页 ----------
publish_site() {
  echo "==> [site] 上传官网静态页 → $NAS_SITE_DIR"
  # -O：群晖没开 SFTP 子系统，不加这个参数会 subsystem request failed
  scp -O -P "$NAS_PORT" \
    "$ROOT/website/index.html" \
    "$ROOT/website/ai-plugin.html" \
    "$ROOT/website/styles.css" \
    "$NAS_HOST:$NAS_SITE_DIR/"
  echo "    页面已传"

  if [ -d "$ROOT/website/public" ]; then
    ssh -p "$NAS_PORT" "$NAS_HOST" "mkdir -p '$NAS_SITE_DIR/public'"
    # 用通配符逐个传，**不要**写 `-r 源目录/.`：新版 OpenSSH 在 legacy(-O) 模式下
    # 会拒绝这种写法并报 `unexpected filename: .`；写 `-r 源目录` 又会在远端已存在
    # 同名目录时套出 public/public。public 下目前只有图片、没有子目录，通配符最稳。
    scp -O -P "$NAS_PORT" "$ROOT"/website/public/* "$NAS_HOST:$NAS_SITE_DIR/public/"
    echo "    public/ 已传"
  fi

  echo "==> [site] 核产物"
  ssh -p "$NAS_PORT" "$NAS_HOST" "ls -la '$NAS_SITE_DIR'"
  if [ -n "$SITE_URL" ]; then
    # 只取状态码，不套管道判成败（curl 自己的退出码要留着）
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$SITE_URL/")
    echo "    GET $SITE_URL/ -> $code"
    [ "$code" = "200" ] || { echo "✗ 官网首页没有返回 200" >&2; exit 1; }
  fi
  echo "✓ [site] 完成"
  echo ""
}

# ---------- 安装包 ----------
publish_app() {
  : "${ITOOLS_DEFAULT_ENDPOINT:?.publish.env 里缺 ITOOLS_DEFAULT_ENDPOINT（自建服务版必须注入）}"
  export ITOOLS_DEFAULT_ENDPOINT

  echo "==> [app] 清理旧产物（仅 release）"
  # 必须 clean：option_env! 编译期展开 + build.rs 无 rerun-if-env-changed，
  # 不 clean 就会沿用上一次编进去的地址（见文件头注意事项 1）。
  #
  # 加 --release 只清发布产物：不带它会连 target/debug 一起删，而那边的
  # itools.exe 常被正在调试运行的实例占着，clean 会以「拒绝访问 (os error 5)」
  # 整个失败——发布根本不需要动 debug 产物。
  (cd "$ROOT/src-tauri" && cargo clean -p itools --release)

  echo "==> [app] 构建（注入服务端地址）"
  (cd "$ROOT" && npm run tauri build)

  SETUP_SRC="$ROOT/src-tauri/target/release/bundle/nsis/iTools_${VERSION}_x64-setup.exe"
  EXE_SRC="$ROOT/src-tauri/target/release/itools.exe"
  [ -f "$SETUP_SRC" ] || { echo "✗ 没找到安装包 $SETUP_SRC" >&2; exit 1; }

  echo "==> [app] 校验 NSIS 钩子真的编进去了"
  # 钩子（src-tauri/windows/hooks.nsh）负责装新版前卸掉历史遗留的 MSI 安装——
  # 它要是没编进去，安装器外观完全正常，用户那边却会留下两条卸载记录 + 两个桌面图标，
  # 正是本次要修的那个 bug 原样复发。
  #
  # 校验对象是 bundler 生成的**明文中间脚本**，不是安装包：安装包被 LZMA 整体压缩，
  # grep 任何字符串都搜不到（与上面「校验裸 exe 而非安装包」是同一个道理）。
  # 这个中间脚本正好补上了那道防呆够不着的空档。
  NSI_SRC="$ROOT/src-tauri/target/release/nsis/x64/installer.nsi"
  [ -f "$NSI_SRC" ] || { echo "✗ 没找到中间脚本 $NSI_SRC" >&2; exit 1; }
  if grep -q -F 'hooks.nsh' "$NSI_SRC"; then
    echo "    ✓ installer.nsi 里 include 了 hooks.nsh"
  else
    echo "✗ 安装器没有 include windows/hooks.nsh——旧 MSI 清理逻辑没编进去，别发这个包" >&2
    echo "  多半是 tauri.conf.json 的 bundle.windows.nsis.installerHooks 丢了，或该文件没进版本库" >&2
    exit 1
  fi

  echo "==> [app] 校验发行线指纹是 selfhost"
  # 两条发行线的**反向校验**：官网线发出去的包必须自报 selfhost，GitHub 线的必须自报 oss。
  # 光校验「含端点」不够——它只能证明这一次注入成功，证明不了产物没被拿去当开源包发。
  # 指纹由 src-tauri/src/updater.rs 的 CHANNEL_MARK 在编译期定死，启动日志每次都会打印它。
  if grep -a -q -F 'ITOOLS_RELEASE_CHANNEL=selfhost' "$EXE_SRC"; then
    echo "    ✓ 产物自报 selfhost（官网线）"
  else
    echo "✗ 产物不是官网线（找不到 ITOOLS_RELEASE_CHANNEL=selfhost）——这个包别发到官网" >&2
    echo "  多半是 ITOOLS_DEFAULT_ENDPOINT 没进构建；发出去会让用户的云服务接入直接失效" >&2
    exit 1
  fi

  echo "==> [app] 校验地址真的注入了"
  # 见文件头注意事项 2：不校验的话，发出去的包连不上服务器且外观完全正常。
  #
  # ⚠ 校验的是**裸 exe**（$EXE_SRC）而不是 NSIS 安装包——别改成校验安装包：
  #    NSIS 把内容 LZMA 压缩了，对安装包 grep 任何字符串都搜不到，于是无论注入
  #    成没成都会得出「不含」的结论。2026-08-18 就靠这个差点误判过一次：当时对
  #    自建版安装包 grep 不到地址，险些当成注入失败——实际是裸 exe 里明明有。
  #    真要验安装包，得先 7z 解开再 grep 里面的 itools.exe。
  if grep -a -q -F "$ITOOLS_DEFAULT_ENDPOINT" "$EXE_SRC"; then
    echo "    ✓ 二进制内含 $ITOOLS_DEFAULT_ENDPOINT"
  else
    echo "✗ 二进制里找不到 $ITOOLS_DEFAULT_ENDPOINT——地址没注入进去" >&2
    echo "  多半是 cargo clean 没生效或环境变量没传进构建，别把这个包发出去" >&2
    exit 1
  fi
  echo "    安装包 $(ls -lh "$SETUP_SRC" | awk '{print $5}')"

  echo "==> [app] 上传 → $NAS_DOWNLOADS_DIR"
  scp -O -P "$NAS_PORT" "$SETUP_SRC" "$NAS_HOST:$NAS_DOWNLOADS_DIR/"
  # 固定名副本：官网下载按钮指向它（相对路径 /download/iTools-latest-setup.exe）。
  # 带版本号的那个文件仍要留着——/api/download/latest 靠扫文件名里的版本号得出版本。
  ssh -p "$NAS_PORT" "$NAS_HOST" \
    "cp '$NAS_DOWNLOADS_DIR/iTools_${VERSION}_x64-setup.exe' '$NAS_DOWNLOADS_DIR/iTools-latest-setup.exe'"

  echo "==> [app] 核产物"
  ssh -p "$NAS_PORT" "$NAS_HOST" "ls -la '$NAS_DOWNLOADS_DIR'"
  if [ -n "$SITE_URL" ]; then
    latest=$(curl -s --max-time 15 "$SITE_URL/api/download/latest")
    echo "    /api/download/latest -> $latest"
    case "$latest" in
      *"\"$VERSION\""*) echo "    ✓ 版本号端点已跟到 $VERSION" ;;
      *) echo "✗ 版本号端点没跟上（期望 $VERSION）：$latest" >&2; exit 1 ;;
    esac
  fi
  echo "✓ [app] 完成"
  echo ""
}

case "$MODE" in
  site) publish_site ;;
  app)  publish_app ;;
  all)  publish_app; publish_site ;;
  *)    echo "✗ 未知模式 '$MODE'，可用：site | app | all" >&2; exit 1 ;;
esac

echo "==> 全部完成（版本 $VERSION）"
[ -n "$SITE_URL" ] && echo "    官网：$SITE_URL/"
