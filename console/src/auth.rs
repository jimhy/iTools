//! 鉴权原语：口令哈希（scrypt，加盐）+ 会话令牌生成与存储哈希。
//!
//! scrypt 参数与 `server/src/auth.rs` **逐位一致**（N=16384、r=8、p=1、keylen=64，
//! 盐取其 hex 字符串的 UTF-8 字节）。两边保持一致不是为了共用哈希——控制台管理员
//! 与终端用户是**完全隔离的两套账号**——而是为了口径统一：将来若要把某个能力
//! 在两边搬移，不会因为 KDF 参数不同而踩坑。测试里钉了同一条 Node 实测向量做回归。
//!
//! 与主服务的一处**有意分歧**：会话令牌**不明文入库**，库里只存 sha256。
//! 主服务的 `sessions.token` 是明文主键，一次拖库即拿到全部在线会话；控制台是
//! 运营入口、权限更高，这里必须收紧。校验时对来访 token 求哈希再查表，
//! 语义完全等价，但库被读走也无法直接复用令牌。

use rand::RngCore;
use scrypt::{scrypt, Params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 派生密钥长度（字节）。
const KEYLEN: usize = 64;
/// log2(N)=14 → N=16384。
const LOG_N: u8 = 14;
const R: u32 = 8;
const P: u32 = 1;

/// 口令哈希 + 盐（都是小写 hex）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash {
    pub hash: String,
    pub salt: String,
}

/// 用给定的盐派生口令哈希（hex）。
pub fn derive(password: &str, salt: &str) -> String {
    let params = Params::new(LOG_N, R, P, KEYLEN).expect("scrypt 参数为编译期常量，必然合法");
    let mut out = vec![0u8; KEYLEN];
    scrypt(password.as_bytes(), salt.as_bytes(), &params, &mut out).expect("scrypt 派生失败");
    hex::encode(out)
}

/// 随机生成盐并派生口令哈希。
pub fn hash_password(password: &str) -> PasswordHash {
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let salt = hex::encode(raw);
    let hash = derive(password, &salt);
    PasswordHash { hash, salt }
}

/// 校验口令是否与存储的哈希匹配（常量时间比较）。
pub fn verify_password(password: &str, salt: &str, expected_hex: &str) -> bool {
    let actual = derive(password, salt);
    let expected = match hex::decode(expected_hex) {
        Ok(v) => v,
        // 库里的哈希不是合法 hex（数据损坏 / 人为改坏）→ 校验失败，绝不放行。
        Err(_) => return false,
    };
    let actual_bytes = hex::decode(&actual).expect("derive 输出恒为合法 hex");
    actual_bytes.len() == expected.len() && bool::from(actual_bytes.ct_eq(&expected))
}

/// 生成随机会话令牌（32 字节 → 64 位 hex）。返回的是**明文令牌**，只发给浏览器一次。
pub fn generate_token() -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

/// 令牌的存储形态：sha256(hex)。库里只存这个。
///
/// 这里不加盐、不用慢哈希是**刻意的**：令牌本身就是 32 字节的高熵随机数，
/// 字典攻击无从谈起，而每个请求都要算一次，慢哈希会直接变成 DoS 面。
/// （口令不同——口令是人选的、低熵，所以那边必须用 scrypt。）
pub fn token_digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// 口令强度的最低要求。返回 `Err(理由)` 时应原样展示给管理员。
///
/// 只做**长度与字符类别**这类客观检查，不搞「必须含特殊字符」那套反而促使
/// 用户写 `Passw0rd!` 的规则。
pub fn check_password_strength(password: &str) -> Result<(), String> {
    let len = password.chars().count();
    if len < 8 {
        return Err("口令至少 8 个字符".to_string());
    }
    if len > 128 {
        return Err("口令最多 128 个字符".to_string());
    }
    let has_alpha = password.chars().any(|c| c.is_alphabetic());
    let has_other = password.chars().any(|c| !c.is_alphabetic());
    if !(has_alpha && has_other) {
        return Err("口令需要同时包含字母与非字母字符（数字或符号）".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 `server/src/auth.rs` 的同一条 Node 实测向量。两边 KDF 参数一旦漂移，这里就炸。
    #[test]
    fn scrypt_matches_server_vector() {
        let salt = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        assert_eq!(
            derive("s3cret", salt),
            "5c5dd2258b7261efcfff573b6c955f2c687d24c86896d108d540e66fe2aca383\
             da388d177290f940e7cfdbcaa4f72cdb81cbaa1dd90c08ca8bd35dbb4fa7aca5",
            "scrypt 参数必须与 server/src/auth.rs 逐位一致"
        );
    }

    #[test]
    fn hash_then_verify_roundtrip() {
        let h = hash_password("s3cret-2026");
        assert_eq!(h.salt.len(), 32);
        assert_eq!(h.hash.len(), KEYLEN * 2);
        assert!(verify_password("s3cret-2026", &h.salt, &h.hash));
        assert!(!verify_password("wrong", &h.salt, &h.hash), "错口令必须失败");
    }

    #[test]
    fn salt_is_random_per_admin() {
        let a = hash_password("same-password-1");
        let b = hash_password("same-password-1");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn corrupted_hash_rejects_instead_of_panicking() {
        let h = hash_password("s3cret-2026");
        assert!(!verify_password("s3cret-2026", &h.salt, "不是 hex"));
        assert!(!verify_password("s3cret-2026", &h.salt, "abcd"), "长度不等必须判失败");
    }

    #[test]
    fn token_is_random_and_digest_is_stable() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "令牌必须每次不同");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(token_digest(&a), token_digest(&a), "同一令牌摘要必须稳定");
        assert_ne!(token_digest(&a), token_digest(&b));
        assert_eq!(token_digest(&a).len(), 64, "sha256 hex 长度");
        assert_ne!(token_digest(&a), a, "库里存的绝不能等于明文令牌");
    }

    #[test]
    fn password_strength_rules() {
        assert!(check_password_strength("abc").is_err(), "太短");
        assert!(check_password_strength("abcdefghij").is_err(), "纯字母不行");
        assert!(check_password_strength("1234567890").is_err(), "纯数字不行");
        assert!(check_password_strength("console2026").is_ok());
        assert!(check_password_strength("口令测试 2026").is_ok(), "中文口令按字符计数");
        assert!(check_password_strength(&"a1".repeat(100)).is_err(), "超长必须拒绝");
    }
}
