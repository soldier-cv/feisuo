use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use std::path::PathBuf;
use crate::config::AppConfig;
use crate::error::{FeisuoError, Result};

pub struct DeviceIdentity {
    pub signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
    pub device_id: String,
}

impl DeviceIdentity {
    pub fn load_or_generate() -> Result<Self> {
        Self::load_or_generate_at(Self::key_file_path())
    }

    /// 在指定路径加载/生成设备身份。
    /// 抽出来是为了让"同一进程内跑两个节点"的集成测试可以各自持有独立私钥。
    pub fn load_or_generate_at(key_path: std::path::PathBuf) -> Result<Self> {
        // 读取已存在的私钥; 长度/内容异常时备份损坏文件并重新生成身份,
        // 而不是让整个应用陷入"永久性硬失败"。
        let signing_key = if key_path.exists() {
            match std::fs::read(&key_path)
                .ok()
                .filter(|b| b.len() == 32)
                .and_then(|bytes| {
                    let mut key_bytes = [0u8; 32];
                    key_bytes.copy_from_slice(&bytes);
                    Some(SigningKey::from_bytes(&key_bytes))
                }) {
                Some(key) => key,
                None => {
                    let backup = key_path.with_extension("key.corrupt");
                    let _ = std::fs::rename(&key_path, &backup);
                    tracing::warn!(
                        "设备私钥文件损坏, 已备份为 {} 并重新生成新的设备身份",
                        backup.display()
                    );
                    Self::generate_and_persist(&key_path)?
                }
            }
        } else {
            Self::generate_and_persist(&key_path)?
        };

        let verifying_key = signing_key.verifying_key();
        let pub_hex = hex::encode(verifying_key.to_bytes());
        let device_id = Self::device_id_from_pubkey_hex(&pub_hex)?;

        Ok(Self {
            signing_key,
            verifying_key,
            device_id,
        })
    }

    /// 原子地生成并落盘新的设备私钥 (先写临时文件再 rename, 避免写一半被杀导致身份彻底损坏)
    fn generate_and_persist(key_path: &PathBuf) -> Result<SigningKey> {
        let mut csprng = OsRng;
        let key = SigningKey::generate(&mut csprng);

        // 必须建 **key_path 自己的父目录**, 不能建 `AppConfig::get_app_dir()`。
        // 旧实现建的是全局默认目录, 而写的是调用方传进来的路径 —— 两者可以
        // 完全无关, 于是:
        // - Android 宿主注入 filesDir 时, 父目录反而没被创建, 落盘直接失败;
        // - 集成测试用临时 key_path 时, 会在**用户真实目录**
        //   (%LOCALAPPDATA%\feisuo) 里凭空建出一套空脚手架,
        //   测试跑完还留在那儿, 属于污染用户环境。
        if let Some(parent) = key_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let tmp_path = key_path.with_extension("key.tmp");
        std::fs::write(&tmp_path, key.to_bytes())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
        }

        std::fs::rename(&tmp_path, key_path)?;
        Ok(key)
    }

    /// 设备指纹完全由公钥派生 —— 接收端必须用同一个函数校验,
    /// 否则攻击者可以拿一个"已信任的 device_id + 自己的公钥"顶替既有身份。
    pub fn device_id_from_pubkey_hex(pub_hex: &str) -> Result<String> {
        let bytes = hex::decode(pub_hex)
            .map_err(|e| FeisuoError::Security(format!("Invalid public key hex: {}", e)))?;
        if bytes.len() != 32 {
            return Err(FeisuoError::Security("Public key must be 32 bytes".into()));
        }
        Ok(format!("feisuo-{}", &pub_hex[..12]))
    }

    fn key_file_path() -> PathBuf {
        AppConfig::get_app_dir().join("device_identity.key")
    }

    /// 轻量读取当前身份指纹 (不解私钥), 供需要稳定设备名的场合使用。
    /// 失败返回 None, 调用方应自行兜底。
    pub fn current_device_id_hint() -> Option<String> {
        let bytes = std::fs::read(Self::key_file_path()).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        let key = SigningKey::from_bytes(&arr);
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        Some(format!("feisuo-{}", &pub_hex[..12]))
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.verifying_key.to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> String {
        let sig: Signature = self.signing_key.sign(message);
        hex::encode(sig.to_bytes())
    }

    pub fn verify(pubkey_hex: &str, message: &[u8], signature_hex: &str) -> Result<bool> {
        let pub_bytes = hex::decode(pubkey_hex)
            .map_err(|e| FeisuoError::Security(format!("Invalid public key hex: {}", e)))?;
        if pub_bytes.len() != 32 {
            return Err(FeisuoError::Security("Public key must be 32 bytes".into()));
        }
        let mut key_arr = [0u8; 32];
        key_arr.copy_from_slice(&pub_bytes);
        let verifying_key = VerifyingKey::from_bytes(&key_arr)
            .map_err(|e| FeisuoError::Security(format!("Corrupt verifying key: {}", e)))?;

        let sig_bytes = hex::decode(signature_hex)
            .map_err(|e| FeisuoError::Security(format!("Invalid signature hex: {}", e)))?;
        if sig_bytes.len() != 64 {
            return Err(FeisuoError::Security("Signature must be 64 bytes".into()));
        }
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);

        Ok(verifying_key.verify(message, &signature).is_ok())
    }
}
