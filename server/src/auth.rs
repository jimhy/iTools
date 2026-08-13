//! 鉴权原语：口令哈希（scrypt，加盐）+ 会话令牌生成。
//!
//! 安全：口令**从不明文存储**——存 scrypt 派生哈希 + 每用户随机盐；校验用常量时间比较防时序侧信道。
//!
//! **参数与旧 Node 版逐位对齐**（`crypto.scryptSync` 的默认值：N=16384、r=8、p=1、keylen=64，
//! salt 取其 hex 字符串的 UTF-8 字节）。因此**老库里已有的用户口令哈希可以直接沿用**，
//! 换成 Rust 服务端后不需要任何用户重设密码。`auth::tests` 里钉了一条 Node 实测出的向量做回归。

use rand::RngCore;
use scrypt::{scrypt, Params};
use subtle::ConstantTimeEq;

/// 派生密钥长度（字节）。
const KEYLEN: usize = 64;
/// log2(N)=14 → N=16384，与 Node `crypto.scryptSync` 默认一致。
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
    // scrypt 只在参数非法时返回 Err（输出长度为 0 等），此处参数固定合法。
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
        // 库里的哈希不是合法 hex（人为改坏 / 数据损坏）→ 校验失败，绝不放行。
        Err(_) => return false,
    };
    let actual_bytes = hex::decode(&actual).expect("derive 输出恒为合法 hex");
    // 长度不等直接失败（常量时间比较要求等长）
    actual_bytes.len() == expected.len() && bool::from(actual_bytes.ct_eq(&expected))
}

/// 生成随机会话令牌（hex）。
pub fn generate_token(bytes: usize) -> String {
    let n = bytes.clamp(16, 256);
    let mut raw = vec![0u8; n];
    rand::rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与旧 Node 版的兼容性回归：这条向量由 Node 实测得出
    /// （`crypto.scryptSync("s3cret", "a1b2c3d4e5f60718293a4b5c6d7e8f90", 64).toString("hex")`）。
    /// 它一旦对不上，就说明换 Rust 后老用户会集体登录失败——必须在测试里炸，而不是上线后才发现。
    #[test]
    fn scrypt_matches_node_vector() {
        // 向量取自 Node 实测：crypto.scryptSync(<口令>, <盐>, 64).toString("hex")
        let salt = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        assert_eq!(
            derive("s3cret", salt),
            "5c5dd2258b7261efcfff573b6c955f2c687d24c86896d108d540e66fe2aca383\
             da388d177290f940e7cfdbcaa4f72cdb81cbaa1dd90c08ca8bd35dbb4fa7aca5",
            "scrypt 派生结果必须与 Node crypto.scryptSync 默认参数一致，否则老用户会集体登录失败"
        );
        // 非 ASCII 口令与盐同样按 UTF-8 字节处理（Node 侧行为一致）
        assert_eq!(
            derive("海风哥的口令 π", "盐 salt"),
            "273b3e9e0b7dbb95c30ab66a280ef293a30312c3e31c50b38f0ad6a0177ced43\
             281d0832ab280dd75a5078d1893af1d67d682ad8825f1dde23b28ee17a4e1e9f"
        );
    }

    #[test]
    fn hash_then_verify_roundtrip() {
        let h = hash_password("s3cret");
        assert_eq!(h.salt.len(), 32, "16 字节盐的 hex 长度");
        assert_eq!(h.hash.len(), KEYLEN * 2);
        assert!(verify_password("s3cret", &h.salt, &h.hash));
        assert!(!verify_password("wrong", &h.salt, &h.hash), "错口令必须失败");
    }

    #[test]
    fn salt_is_random_per_user() {
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a.salt, b.salt, "每用户随机盐");
        assert_ne!(a.hash, b.hash, "同口令不同盐 → 哈希不同");
    }

    #[test]
    fn corrupted_hash_rejects_instead_of_panicking() {
        let h = hash_password("s3cret");
        assert!(!verify_password("s3cret", &h.salt, "不是 hex"));
        assert!(!verify_password("s3cret", &h.salt, "abcd"), "长度不等必须判失败");
    }

    #[test]
    fn token_is_random_hex_of_requested_length() {
        let a = generate_token(32);
        let b = generate_token(32);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 越界值被夹紧到安全区间，不会生成 2 字节的可爆破令牌
        assert_eq!(generate_token(1).len(), 32);
    }
}
