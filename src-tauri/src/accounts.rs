//! 账号系统 —— 阶段三。
//!
//! 架构文档铁律之一：**节点身份 ≠ 用户身份**。P2P 网络层只认 `peer_id`，
//! 从第一天就没有用户字段；账号是后来"绑定"上去的一层。这个模块就是那一层，
//! 它完全不碰 `peer.rs` / `transfer.rs` —— 传输逻辑零改动。
//!
//! 职责：注册、登录、token 签发与校验，账号持久化。
//!
//! # 安全说明（诚实标注）
//!
//! - 密码用 **每用户独立随机盐 + PBKDF2-HMAC-SHA256（10 万轮）** 派生存储，
//!   不存明文、不存裸哈希。这远强于裸 SHA（抗彩虹表、抗暴力破解）。
//! - 但 PBKDF2 **不如 Argon2 抗 GPU/ASIC 并行破解**。选它是为了不引入新依赖
//!   （复用已有的 sha2）。生产环境应换成 Argon2id。
//! - Tracker 协议目前是**明文 TCP**，密码在传输中未加密。仅适用于本机/局域网
//!   的学习场景；公网部署必须套 TLS。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// PBKDF2 迭代轮数。越高越抗暴力破解，代价是登录变慢。
const PBKDF2_ITERATIONS: u32 = 100_000;
/// 盐长度（字节）。
const SALT_LEN: usize = 16;
/// 派生密钥长度（字节）—— 一个 SHA-256 输出。
const DK_LEN: usize = 32;
/// token 有效期：7 天。
const TOKEN_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// SHA-256 的块大小，HMAC 需要。
const SHA256_BLOCK: usize = 64;

/// HMAC-SHA256。标准实现：key 长于块则先哈希，然后 ipad/opad 两轮。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        let digest = Sha256::digest(key);
        k[..32].copy_from_slice(&digest);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; SHA256_BLOCK];
    let mut opad = [0x5cu8; SHA256_BLOCK];
    for i in 0..SHA256_BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let out = outer.finalize();

    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

/// PBKDF2-HMAC-SHA256，输出 32 字节。
/// DK_LEN 正好等于一个 HMAC 输出，所以只需要一个 block（i=1）。
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; DK_LEN] {
    // U1 = HMAC(password, salt || INT_32_BE(1))
    let mut msg = Vec::with_capacity(salt.len() + 4);
    msg.extend_from_slice(salt);
    msg.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &msg);
    let mut out = u;
    // T = U1 xor U2 xor ... xor Uc
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for i in 0..DK_LEN {
            out[i] ^= u[i];
        }
    }
    out
}

/// 常量时间比较，避免通过响应时间泄漏信息。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 生成 n 字节随机数据。
///
/// 不引入 rand crate —— 用系统时间、进程 id、堆地址等熵源喂给 SHA-256。
/// 对盐（只需唯一性）足够；对 token 也够用于本项目场景，但生产环境
/// 应改用 OS CSPRNG（getrandom / rand crate）。
fn random_bytes(n: usize) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut hasher = Sha256::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(now.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    // 一个堆分配的地址，引入 ASLR 熵。
    let boxed = Box::new(0u8);
    hasher.update((&*boxed as *const u8 as usize).to_le_bytes());

    let mut out = Vec::with_capacity(n);
    let mut seed = hasher.finalize().to_vec();
    while out.len() < n {
        seed = Sha256::digest(&seed).to_vec();
        out.extend_from_slice(&seed);
    }
    out.truncate(n);
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一个账号记录。密码只以派生结果存储。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Account {
    username: String,
    /// 随机盐（hex）。
    salt: String,
    /// PBKDF2 派生结果（hex）。
    hash: String,
    created_at: u64,
}

/// 一个已签发的 token。
#[derive(Debug, Clone)]
struct TokenEntry {
    username: String,
    expires_at: u64,
}

/// 账号存储 —— Tracker 侧持有。
///
/// 账号持久化到 JSON；token 只在内存（Tracker 重启后失效，用户重新登录即可，
/// 这也更安全：不会有长期有效的 token 留在磁盘上）。
pub struct Accounts {
    path: Option<PathBuf>,
    accounts: Mutex<HashMap<String, Account>>,
    tokens: Mutex<HashMap<String, TokenEntry>>,
}

/// 登录/注册成功的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    pub token: String,
    pub username: String,
}

impl Accounts {
    /// 纯内存账号库（测试用）。
    pub fn new() -> Self {
        Self {
            path: None,
            accounts: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// 从 JSON 文件打开。不存在视为空库；损坏则备份为 .bak 并以空库启动。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let accounts = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<Account>>(&text) {
                Ok(list) => list
                    .into_iter()
                    .map(|a| (a.username.clone(), a))
                    .collect::<HashMap<_, _>>(),
                Err(e) => {
                    eprintln!("[accounts] {} is corrupt ({e}), starting empty", path.display());
                    let _ = fs::rename(&path, path.with_extension("json.bak"));
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self {
            path: Some(path),
            accounts: Mutex::new(accounts),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// 注册新账号。用户名已存在或输入不合法时返回 Err。
    pub fn register(&self, username: &str, password: &str) -> Result<AuthResult, String> {
        let username = username.trim();
        if username.is_empty() || username.len() > 32 {
            return Err("用户名需为 1-32 个字符".into());
        }
        if password.len() < 6 {
            return Err("密码至少 6 位".into());
        }

        {
            let accounts = self.accounts.lock().unwrap();
            if accounts.contains_key(username) {
                return Err("用户名已被占用".into());
            }
        }

        let salt = random_bytes(SALT_LEN);
        let dk = pbkdf2(password.as_bytes(), &salt, PBKDF2_ITERATIONS);
        let account = Account {
            username: username.to_string(),
            salt: hex::encode(&salt),
            hash: hex::encode(dk),
            created_at: now_secs(),
        };

        self.accounts
            .lock()
            .unwrap()
            .insert(username.to_string(), account);
        self.save();

        Ok(self.issue_token(username))
    }

    /// 登录。用户名不存在或密码错误都返回同一个模糊错误，避免泄漏用户是否存在。
    pub fn login(&self, username: &str, password: &str) -> Result<AuthResult, String> {
        let username = username.trim();
        let account = {
            let accounts = self.accounts.lock().unwrap();
            accounts.get(username).cloned()
        };

        let Some(account) = account else {
            return Err("用户名或密码错误".into());
        };

        let salt = hex::decode(&account.salt).map_err(|_| "账号数据损坏".to_string())?;
        let expected = hex::decode(&account.hash).map_err(|_| "账号数据损坏".to_string())?;
        let actual = pbkdf2(password.as_bytes(), &salt, PBKDF2_ITERATIONS);

        if !constant_time_eq(&actual, &expected) {
            return Err("用户名或密码错误".into());
        }

        Ok(self.issue_token(username))
    }

    /// 签发一个新 token。
    fn issue_token(&self, username: &str) -> AuthResult {
        let token = hex::encode(random_bytes(32));
        self.tokens.lock().unwrap().insert(
            token.clone(),
            TokenEntry {
                username: username.to_string(),
                expires_at: now_secs() + TOKEN_TTL_SECS,
            },
        );
        AuthResult {
            token,
            username: username.to_string(),
        }
    }

    /// 校验 token，返回对应用户名。过期或无效返回 None。
    /// 顺带清理已过期的条目。
    pub fn verify(&self, token: &str) -> Option<String> {
        let now = now_secs();
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, e| e.expires_at > now);
        tokens.get(token).map(|e| e.username.clone())
    }

    /// 登出：吊销 token。
    pub fn logout(&self, token: &str) -> bool {
        self.tokens.lock().unwrap().remove(token).is_some()
    }

    /// 已注册账号数（调试/展示用）。
    #[allow(dead_code)]
    pub fn account_count(&self) -> usize {
        self.accounts.lock().unwrap().len()
    }

    /// 账号列表写盘。原子写：先 .tmp 再 rename。
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let list: Vec<Account> = self.accounts.lock().unwrap().values().cloned().collect();
        let json = match serde_json::to_string_pretty(&list) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[accounts] serialize failed: {e}");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_err() {
            return;
        }
        if fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl Default for Accounts {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracker 账号文件的默认名。
pub const ACCOUNTS_FILE: &str = "tracker_accounts.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 Test Case 1: key = 0x0b*20, data = "Hi There"
        let key = vec![0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex::encode(mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn pbkdf2_is_deterministic_and_salt_sensitive() {
        let a = pbkdf2(b"password", b"salt1", 1000);
        let b = pbkdf2(b"password", b"salt1", 1000);
        let c = pbkdf2(b"password", b"salt2", 1000);
        assert_eq!(a, b, "same inputs must give same output");
        assert_ne!(a, c, "different salt must change output");
    }

    #[test]
    fn register_then_login() {
        let acc = Accounts::new();
        let reg = acc.register("alice", "hunter2!").unwrap();
        assert_eq!(reg.username, "alice");
        assert!(!reg.token.is_empty());

        // 注册即登录：token 立刻可用。
        assert_eq!(acc.verify(&reg.token).as_deref(), Some("alice"));

        // 正确密码可再次登录，拿到新 token。
        let login = acc.login("alice", "hunter2!").unwrap();
        assert_eq!(acc.verify(&login.token).as_deref(), Some("alice"));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let acc = Accounts::new();
        acc.register("bob", "correct-horse").unwrap();
        let err = acc.login("bob", "wrong-password").unwrap_err();
        // 错误信息不区分"用户不存在"与"密码错误"，避免用户名枚举。
        assert_eq!(err, "用户名或密码错误");
        assert_eq!(acc.login("nosuchuser", "whatever").unwrap_err(), err);
    }

    #[test]
    fn duplicate_username_rejected() {
        let acc = Accounts::new();
        acc.register("carol", "password1").unwrap();
        assert!(acc.register("carol", "password2").is_err());
        // 原密码仍然有效，没被覆盖。
        assert!(acc.login("carol", "password1").is_ok());
    }

    #[test]
    fn input_validation() {
        let acc = Accounts::new();
        assert!(acc.register("", "password1").is_err(), "empty username");
        assert!(acc.register("dave", "short").is_err(), "short password");
        assert!(acc.register(&"x".repeat(33), "password1").is_err(), "long username");
    }

    #[test]
    fn logout_revokes_token() {
        let acc = Accounts::new();
        let auth = acc.register("erin", "password1").unwrap();
        assert!(acc.verify(&auth.token).is_some());

        assert!(acc.logout(&auth.token));
        assert!(acc.verify(&auth.token).is_none(), "token must be revoked");
        // 重复登出返回 false。
        assert!(!acc.logout(&auth.token));
    }

    #[test]
    fn expired_token_is_rejected() {
        let acc = Accounts::new();
        let auth = acc.register("frank", "password1").unwrap();
        // 手动把过期时间改到过去，模拟 TTL 到期。
        {
            let mut tokens = acc.tokens.lock().unwrap();
            if let Some(e) = tokens.get_mut(&auth.token) {
                e.expires_at = now_secs().saturating_sub(1);
            }
        }
        assert!(acc.verify(&auth.token).is_none(), "expired token must fail");
    }

    #[test]
    fn invalid_token_is_rejected() {
        let acc = Accounts::new();
        acc.register("grace", "password1").unwrap();
        assert!(acc.verify("not-a-real-token").is_none());
    }

    #[test]
    fn accounts_persist_across_reopen() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("music_acc_test_{}_{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join(ACCOUNTS_FILE);

        {
            let acc = Accounts::open(&path);
            acc.register("henry", "password1").unwrap();
        } // 模拟 Tracker 退出

        {
            let reopened = Accounts::open(&path);
            assert_eq!(reopened.account_count(), 1, "account lost across reopen");
            // 密码校验在重启后仍然工作（盐和派生结果都正确持久化了）。
            assert!(reopened.login("henry", "password1").is_ok());
            assert!(reopened.login("henry", "wrong").is_err());
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
