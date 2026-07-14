# iTools 云同步服务端镜像：Node + tsx 直跑 TS。配置全走环境变量（见 .env.example / README）。
FROM node:22-alpine
WORKDIR /app

# 先装依赖（含 tsx，用它直接跑 TS），利用层缓存
COPY package.json package-lock.json ./
RUN npm ci --registry=https://registry.npmmirror.com

# 源码
COPY tsconfig.json ./
COPY src ./src

# 默认监听所有网卡（容器内）；实际端口/DB 连接由运行时环境变量给定
ENV SYNC_HOST=0.0.0.0
EXPOSE 7010

CMD ["npx", "tsx", "src/index.ts"]
