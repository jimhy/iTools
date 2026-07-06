//! 服务端配置：全部从环境变量读取，均有合理默认。凭据/密钥不写死在源码里。

export interface Config {
  host: string;
  port: number;
  /** 数据持久化文件路径 */
  dataFile: string;
  /** 是否允许「首次登录即自动注册」（自托管默认开） */
  allowRegister: boolean;
  /** 会话令牌随机字节数 */
  tokenBytes: number;
  /** 是否启用 fastify 访问日志 */
  logger: boolean;
}

function boolEnv(v: string | undefined, dflt: boolean): boolean {
  if (v == null) return dflt;
  return !["false", "0", "no", "off"].includes(v.trim().toLowerCase());
}

/** 从环境变量装载配置（可传入自定义 env 便于测试）。 */
export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  return {
    host: env.SYNC_HOST ?? "127.0.0.1",
    port: Number(env.SYNC_PORT ?? env.PORT ?? 8787),
    dataFile: env.SYNC_DATA_FILE ?? "./data/db.json",
    allowRegister: boolEnv(env.SYNC_ALLOW_REGISTER, true),
    tokenBytes: Number(env.SYNC_TOKEN_BYTES ?? 32),
    logger: boolEnv(env.SYNC_LOG, true),
  };
}
