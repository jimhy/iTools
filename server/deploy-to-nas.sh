#!/bin/sh
# 部署 itools-sync-server 到自建服务器（群晖 NAS）。
#
# # 为什么在本地编译
#
# 群晖那颗 CPU 编译 Rust 太慢（一次全量十几分钟起步），而本机几分钟就完事。
# 所以流程是「本地 docker build → save → 传镜像 → load」，**服务器上不做任何编译**。
# 传的是 30 多 MB 的压缩镜像，比传源码再就地编译快一个数量级。
#
# # 用法
#
#   export NAS_HOST=user@your-nas-host     # 必填
#   export NAS_PORT=22                     # 可选，默认 22
#   ./deploy-to-nas.sh [镜像tag]           # tag 默认 rust-new4
#
# 地址**只从环境变量读**：这个仓库是公开的，运维拓扑不进源码
# （与 .github/workflows/release.yml 里那条「公开仓库不暴露运维拓扑」同一条原则）。
#
# # 前置
#
# - 本机 Docker 可用，且有 rust:1-bookworm 与 debian:bookworm-slim 两个基础镜像
#   （没有就 docker pull；rust 的次版本镜像可以 docker tag 顶上）。
# - 目标机上已有 run-new*.sh 那样的启动脚本（含数据库口令等凭据，**只存在于服务器上**，
#   绝不进仓库）。本脚本不生成它，只调用。
# - SSH 免密（公钥已在目标机 authorized_keys）。
#
# # 注意
#
# 判断每一步成没成**不套管道**——管道会把退出码顶成末端进程的，
# 曾经因此把「构建失败」当成功交付过。每步单独判退出码，最后再核产物。

set -e

TAG="${1:-rust-new4}"
IMAGE="itools-sync-server:$TAG"
: "${NAS_HOST:?请先 export NAS_HOST=user@host}"
NAS_PORT="${NAS_PORT:-22}"
REMOTE_DIR="${NAS_REMOTE_DIR:-/volume1/docker/itools-build}"
RUN_SCRIPT="${NAS_RUN_SCRIPT:-$REMOTE_DIR/run-new3.sh}"

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TARBALL="${TMPDIR:-/tmp}/itools-$TAG.tar"

echo "==> [1/5] 本地构建镜像 $IMAGE"
docker build --build-arg CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}" -t "$IMAGE" "$SCRIPT_DIR"

echo "==> [2/5] 导出并压缩"
rm -f "$TARBALL" "$TARBALL.gz"
docker save -o "$TARBALL" "$IMAGE"
gzip -f "$TARBALL"
echo "    $(ls -lh "$TARBALL.gz" | awk '{print $5}')"

echo "==> [3/5] 上传到 $NAS_HOST"
# -O 走 legacy scp 协议：群晖默认没开 SFTP 子系统，不加这个参数会 subsystem request failed
scp -O -P "$NAS_PORT" "$TARBALL.gz" "$NAS_HOST:$REMOTE_DIR/"

echo "==> [4/5] 远端 load 镜像"
ssh -p "$NAS_PORT" "$NAS_HOST" "sudo /usr/local/bin/docker load -i $REMOTE_DIR/$(basename "$TARBALL").gz"

echo "==> [5/5] 切换容器（会有约一分钟服务中断）"
ssh -p "$NAS_PORT" "$NAS_HOST" "$RUN_SCRIPT"

echo "==> 核产物：容器状态与挂载日志"
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "sudo /usr/local/bin/docker ps --filter name=itools-server --format '{{.Names}} | {{.Image}} | {{.Status}}'"
ssh -p "$NAS_PORT" "$NAS_HOST" \
  "sudo /usr/local/bin/docker logs itools-server 2>&1 | grep -iE '挂载|listen' | tail -5"

rm -f "$TARBALL.gz"
echo "==> 完成。别忘了实测一次 /health、/ 与 /download/<安装包>。"
