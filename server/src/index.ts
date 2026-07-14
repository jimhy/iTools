//! 入口：装载配置 → 连接 MariaDB → 起 HTTP 服务。

import { loadConfig } from "./config";
import { MariaDbStore } from "./store";
import { buildServer } from "./server";

async function main(): Promise<void> {
  const config = loadConfig();
  const store = new MariaDbStore(config.db);

  // 诚实：连不上 DB 就别假装起来了——打印清晰错误并退出。
  try {
    await store.init();
  } catch (err) {
    console.error(
      `[itools-sync] 连接 MariaDB 失败（${config.db.user}@${config.db.host}:${config.db.port}/${config.db.database}）：`,
      (err as Error).message,
    );
    console.error("请检查 SYNC_DB_HOST/PORT/USER/PASSWORD/NAME 环境变量与数据库可达性。");
    process.exit(1);
  }

  const app = buildServer(store, config);
  await app.listen({ host: config.host, port: config.port });

  if (!config.logger) {
    // 日志关闭时也给一行启动提示（协议随是否启用 TLS 而定，避免 HTTPS 下误显 http）
    const scheme = config.tls ? "https" : "http";
    console.log(`itools-sync 已启动: ${scheme}://${config.host}:${config.port}`);
  }
}

main().catch((err) => {
  console.error("启动失败:", err);
  process.exit(1);
});
