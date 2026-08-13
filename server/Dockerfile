# iTools 云同步服务端镜像：Rust 静态编译 + 极简运行层。配置全走环境变量（见 .env.example / README）。
#
# 依赖只有 rustls（内置 webpki 根证书），运行层因此不需要 openssl / ca-certificates。

FROM rust:1-bookworm AS builder
WORKDIR /build

# 限制 rustc 并行度：默认按 CPU 核数并发，在 2GB 内存的容器/小 VPS 上编译 axum、sqlx
# 这类大 crate 会直接被 OOM killer 干掉（实测：Docker Desktop 默认内存下 SIGKILL）。
# 内存宽裕的机器可以调大：docker build --build-arg CARGO_BUILD_JOBS=8 ...
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}

# 先只拷清单预热依赖层：源码改动不会让依赖重新编译。
# 注意 Cargo.toml 里 [lib]/[bin] 指定了路径，占位文件两个都要有。
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# 真源码
COPY src ./src
# 让 cargo 确实重编本包（占位文件的 mtime 可能更新）
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /build/target/release/itools-sync-server /usr/local/bin/itools-sync-server
# 镜像源列表（可编辑配置）。运行时可用 -v 挂载覆盖，改完存盘即生效、无需重启（mtime 轮询）。
# 默认路径是「工作目录下的 config/mirrors.json」，即这里的 /app/config/mirrors.json。
COPY config ./config

# 默认监听所有网卡（容器内）；实际端口/DB 连接由运行时环境变量给定
ENV SYNC_HOST=0.0.0.0
EXPOSE 7010

CMD ["itools-sync-server"]
