//! 入口：装载配置 → 打开存储 → 起 HTTP 服务。

import { loadConfig } from "./config";
import { JsonStore } from "./store";
import { buildServer } from "./server";

async function main(): Promise<void> {
  const config = loadConfig();
  const store = new JsonStore(config.dataFile);
  await store.load();

  const app = buildServer(store, config);
  await app.listen({ host: config.host, port: config.port });

  if (!config.logger) {
    // 日志关闭时也给一行启动提示
    console.log(`itools-sync 已启动: http://${config.host}:${config.port}`);
  }
}

main().catch((err) => {
  console.error("启动失败:", err);
  process.exit(1);
});
