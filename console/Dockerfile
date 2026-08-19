# iTools 运营控制台镜像。构建套路与 server/Dockerfile 一致：Rust 静态编译 + 极简运行层。
#
# 依赖只有 rustls（内置 webpki 根证书），运行层因此不需要 openssl / ca-certificates。

FROM rust:1-bookworm AS builder
WORKDIR /build

# 限制 rustc 并行度：默认按 CPU 核数并发，编译 axum、sqlx 这类大 crate 时
# 在 2GB 内存的机器上会被 OOM killer 干掉。内存宽裕时可调大：
#   docker build --build-arg CARGO_BUILD_JOBS=8 ...
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}

# 先只拷清单预热依赖层：源码改动不会让依赖重新编译。
# Cargo.toml 里 [lib]/[bin] 都指定了路径，两个占位文件都要有。
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# 真源码
COPY src ./src
# 占位文件的 mtime 可能更新，touch 一下确保本包真的重编
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /build/target/release/itools-console /usr/local/bin/itools-console

# 前端静态资源打进镜像做兜底：没挂载卷时控制台照样能开。
# 生产建议再用 -v 挂一份到 /app/web —— 前端是零构建的纯静态文件，
# 改完 scp 上去就生效（秒级），不必为改一句文案重发 30MB 镜像。
COPY web ./web
ENV CONSOLE_WEB_DIR=/app/web

# 容器内默认监听所有网卡；实际绑定地址/端口由运行时环境变量给定。
# 生产走 host 网络 + 绑 127.0.0.1，对外由 frp 透传。
ENV CONSOLE_HOST=0.0.0.0
ENV CONSOLE_PORT=7005
EXPOSE 7005

CMD ["itools-console"]
