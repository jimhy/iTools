#!/bin/sh
# 部署 itools-console 到自建服务器（群晖 NAS）。
#
# # 为什么在本地编译
#
# 与 server/deploy-to-nas.sh 同一条理由：群晖那颗 CPU 编 Rust 太慢，
# 而且 2GB 内存编 axum/sqlx 会被 OOM killer 干掉。所以流程是
# 「本地 docker build → save → 传镜像 → load」，**服务器上不做任何编译**。
#
# # 用法
#
#   export NAS_HOST=user@your-nas-host     # 必填
#   export NAS_PORT=22                     # 可选，默认 22
#   export NAS_CERT_DIR=/path/to/certs     # 必填，TLS 证书目录（与云同步服务端共用）
#   ./deploy-to-nas.sh [镜像tag]           # tag 默认 v1
#
# 地址**只从环境变量读**：这个仓库是公开的，运维拓扑不进源码。
#
# # 前置
#
# - 本机 Docker 可用，且有 rust:1-bookworm 与 debian:bookworm-slim 两个基础镜像。
# - 目标机上**已有云同步服务端容器（itools-server）在跑**：本脚本从它的环境变量里
#   读数据库口令，**口令始终留在服务器上，不经过本机、不进仓库**。
# - SSH 免密（公钥已在目标机 authorized_keys）。
#
# # 注意
#
# 判断每一步成没成**不套管道**——管道会把退出码顶成末端进程的，
# server/ 那边曾因此把「构建失败」当成功交付过。每步单独判退出码，最后再核产物。

set -e

TAG="${1:-v1}"
IMAGE="itools-console:$TAG"
: "${NAS_HOST:?请先 export NAS_HOST=user@host}"
NAS_PORT="${NAS_PORT:-22}"
REMOTE_DIR="${NAS_REMOTE_DIR:-/volume1/docker/itools-build}"
# 前端静态资源目录：挂进容器后，改前端只要重传这个目录，不必重发镜像
WEB_DIR="${NAS_CONSOLE_WEB_DIR:-/volume1/docker/itools-data/console-web}"
CERT_DIR="${NAS_CERT_DIR:?请 export NAS_CERT_DIR=<NAS 上的证书目录>（与云同步服务端共用那份）}"
CONSOLE_PORT="${CONSOLE_PORT:-7005}"
UPSTREAM_PORT="${UPSTREAM_PORT:-7101}"

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TARBALL="${TMPDIR:-/tmp}/itools-console-$TAG.tar"
DK=/usr/local/bin/docker

echo "==> [1/7] 本地构建镜像 $IMAGE"
docker build --build-arg CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}" -t "$IMAGE" "$SCRIPT_DIR"

echo "==> [2/7] 导出并压缩"
rm -f "$TARBALL" "$TARBALL.gz"
docker save -o "$TARBALL" "$IMAGE"
gzip -f "$TARBALL"
echo "    $(ls -lh "$TARBALL.gz" | awk '{print $5}')"

echo "==> [3/7] 上传镜像到 $NAS_HOST"
# -O 走 legacy scp 协议：群晖默认没开 SFTP 子系统，不加会 subsystem request failed
scp -O -P "$NAS_PORT" "$TARBALL.gz" "$NAS_HOST:$REMOTE_DIR/"

echo "==> [4/7] 上传前端静态资源到 $WEB_DIR"
# /volume1/docker 是 drwxrwxrwx，建目录不需要 sudo（群晖上 jimhy 的 sudo 只对 docker 免密）
ssh -p "$NAS_PORT" "$NAS_HOST" "mkdir -p $WEB_DIR/js/views"
scp -O -P "$NAS_PORT" "$SCRIPT_DIR/web/index.html" "$SCRIPT_DIR/web/app.css" "$NAS_HOST:$WEB_DIR/"
scp -O -P "$NAS_PORT" "$SCRIPT_DIR"/web/js/*.js "$NAS_HOST:$WEB_DIR/js/"
scp -O -P "$NAS_PORT" "$SCRIPT_DIR"/web/js/views/*.js "$NAS_HOST:$WEB_DIR/js/views/"

echo "==> [5/7] 远端 load 镜像"
ssh -p "$NAS_PORT" "$NAS_HOST" "sudo $DK load -i $REMOTE_DIR/$(basename "$TARBALL").gz"

echo "==> [6/7] 生成启动脚本并切换容器"
# 启动脚本在服务器上就地生成：数据库口令在脚本运行时从 itools-server 容器里读，
# 全程留在服务器上——本机看不到、脚本文件里也没有明文、仓库里更没有。
# 引导管理员的口令由 CONSOLE_BOOTSTRAP_PASSWORD 环境变量传入（可选，只首次需要）。
ssh -p "$NAS_PORT" "$NAS_HOST" "cat > $REMOTE_DIR/run-console.sh" <<REMOTE
#!/bin/sh
# itools-console 启动脚本（由 console/deploy-to-nas.sh 生成，含凭据，勿进仓库）
set -e
DK=$DK
D=$REMOTE_DIR

# 数据库口令：从**正在运行的云同步服务端容器**里读，两者用同一个库。
#
# 为什么不解析启动脚本：那份脚本里的写法是 -e 'SYNC_DB_PASSWORD=值'（整段被单引号包住），
# 按空白切会把结尾的单引号一起带进来，口令就多一个字符、认证必然失败——这个坑踩过一次。
# 容器的 Config.Env 是运行时的权威值，不受脚本书写格式影响。
DB_PASS=\$(sudo \$DK inspect itools-server --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null | grep '^SYNC_DB_PASSWORD=' | cut -d= -f2-)
if [ -z "\$DB_PASS" ]; then
  echo "取不到数据库口令：请确认容器 itools-server 正在运行且设置了 SYNC_DB_PASSWORD" >&2
  exit 1
fi

sudo \$DK stop itools-console 2>/dev/null || true
sudo \$DK rm itools-console 2>/dev/null || true

sudo \$DK run -d --name itools-console \\
  --network host \\
  --restart unless-stopped \\
  -e CONSOLE_HOST=127.0.0.1 \\
  -e CONSOLE_PORT=$CONSOLE_PORT \\
  -e CONSOLE_DB_HOST=127.0.0.1 \\
  -e CONSOLE_DB_PORT=3306 \\
  -e CONSOLE_DB_USER=root \\
  -e CONSOLE_DB_PASSWORD="\$DB_PASS" \\
  -e CONSOLE_DB_NAME=itools \\
  -e CONSOLE_TLS_CERT_FILE=/certs/fullchain.pem \\
  -e CONSOLE_TLS_KEY_FILE=/certs/key.pem \\
  -e CONSOLE_WEB_DIR=/app/web \\
  -e CONSOLE_TZ_OFFSET_MIN=480 \\
  -e CONSOLE_LOGIN_RATE_MAX=10 \\
  -e CONSOLE_LOGIN_RATE_WINDOW_SEC=300 \\
  -e CONSOLE_UPSTREAM_HEALTH_URL=https://127.0.0.1:$UPSTREAM_PORT/health \\
  -e CONSOLE_UPSTREAM_INSECURE=true \\
  \${CONSOLE_BOOTSTRAP_USER:+-e CONSOLE_BOOTSTRAP_USER=\$CONSOLE_BOOTSTRAP_USER} \\
  \${CONSOLE_BOOTSTRAP_PASSWORD:+-e CONSOLE_BOOTSTRAP_PASSWORD=\$CONSOLE_BOOTSTRAP_PASSWORD} \\
  -v $CERT_DIR:/certs:ro \\
  -v $WEB_DIR:/app/web:ro \\
  itools-console:$TAG
REMOTE
# 600：这个脚本会在运行时读出数据库口令，别让同机其它账号看到
ssh -p "$NAS_PORT" "$NAS_HOST" "chmod 700 $REMOTE_DIR/run-console.sh"

# 引导变量只在首次部署时需要，通过环境变量传进去，不写进脚本文件
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "CONSOLE_BOOTSTRAP_USER='${CONSOLE_BOOTSTRAP_USER:-}' CONSOLE_BOOTSTRAP_PASSWORD='${CONSOLE_BOOTSTRAP_PASSWORD:-}' $REMOTE_DIR/run-console.sh"

echo "==> [7/7] 核产物"
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "sudo $DK ps --filter name=itools-console --format '{{.Names}} | {{.Image}} | {{.Status}}'"
# 启动日志里必须能看到监听行；口令类字段一律过滤掉再回显
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "sudo $DK logs itools-console 2>&1 | grep -viE 'password|api_key|secret' | tail -8"
# 真发一次请求确认服务在应答（-k：证书签的是对外域名，连 127.0.0.1 会名称不匹配）
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "curl -sk -o /dev/null -w '  /healthz -> HTTP %{http_code}\n' https://127.0.0.1:$CONSOLE_PORT/healthz"
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "curl -sk -o /dev/null -w '  /  -> HTTP %{http_code}\n' https://127.0.0.1:$CONSOLE_PORT/"
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "curl -sk -o /dev/null -w '  /api/overview 未带令牌 -> HTTP %{http_code}（应为 401）\n' https://127.0.0.1:$CONSOLE_PORT/api/overview"

rm -f "$TARBALL.gz"
echo "==> 完成。外网访问前请确认 frp 已把 $CONSOLE_PORT 透传出去。"
