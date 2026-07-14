//! 服务端配置：全部从环境变量读取，均有合理默认。凭据/密钥不写死在源码里。

/** MariaDB 连接配置（凭据全走环境变量，源码零明文）。 */
export interface DbConfig {
  host: string;
  port: number;
  user: string;
  password: string;
  database: string;
  /** 连接池上限 */
  connectionLimit: number;
}

export interface Config {
  host: string;
  port: number;
  /** MariaDB 连接配置 */
  db: DbConfig;
  /** 是否允许「首次登录即自动注册」（自托管默认开） */
  allowRegister: boolean;
  /** 会话令牌随机字节数 */
  tokenBytes: number;
  /** 是否启用 fastify 访问日志 */
  logger: boolean;
  /**
   * TLS 证书（可选）：配了则以 HTTPS 起服务，直接在本进程做 TLS 终止。
   * 用于「frps 纯 TCP 透传 → 本机做 TLS」的部署（证书/私钥文件路径走环境变量，源码零明文）。
   * 不配则纯 HTTP（置于反向代理之后时用）。
   */
  tls?: { certFile: string; keyFile: string };
}

function boolEnv(v: string | undefined, dflt: boolean): boolean {
  if (v == null) return dflt;
  return !["false", "0", "no", "off"].includes(v.trim().toLowerCase());
}

/** 解析 mysql://user:pass@host:port/db 形式的连接串（各段可缺省）。 */
function parseDbUrl(raw: string): Partial<DbConfig> {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new Error(`SYNC_DB_URL 不是合法的连接串: ${raw}`);
  }
  const out: Partial<DbConfig> = {};
  if (u.hostname) out.host = decodeURIComponent(u.hostname);
  if (u.port) out.port = Number(u.port);
  if (u.username) out.user = decodeURIComponent(u.username);
  // 显式空口令（mysql://root:@host）也算「设置了口令为空」
  if (u.password !== "") out.password = decodeURIComponent(u.password);
  const db = u.pathname.replace(/^\//, "");
  if (db) out.database = decodeURIComponent(db);
  return out;
}

/** 从环境变量装载配置（可传入自定义 env 便于测试）。 */
export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  // 单一 SYNC_DB_URL 存在时覆盖对应的分项配置。
  const url = env.SYNC_DB_URL ? parseDbUrl(env.SYNC_DB_URL) : {};
  const db: DbConfig = {
    host: url.host ?? env.SYNC_DB_HOST ?? "127.0.0.1",
    port: url.port ?? Number(env.SYNC_DB_PORT ?? 3306),
    user: url.user ?? env.SYNC_DB_USER ?? "root",
    password: url.password ?? env.SYNC_DB_PASSWORD ?? "",
    database: url.database ?? env.SYNC_DB_NAME ?? "itools",
    connectionLimit: Number(env.SYNC_DB_CONNLIMIT ?? 10),
  };
  return {
    host: env.SYNC_HOST ?? "127.0.0.1",
    port: Number(env.SYNC_PORT ?? env.PORT ?? 8787),
    db,
    allowRegister: boolEnv(env.SYNC_ALLOW_REGISTER, true),
    tokenBytes: Number(env.SYNC_TOKEN_BYTES ?? 32),
    logger: boolEnv(env.SYNC_LOG, true),
    tls:
      env.SYNC_TLS_CERT_FILE && env.SYNC_TLS_KEY_FILE
        ? { certFile: env.SYNC_TLS_CERT_FILE, keyFile: env.SYNC_TLS_KEY_FILE }
        : undefined,
  };
}
