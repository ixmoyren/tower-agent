//! mod 的落盘存储（`dirs` 统一平台目录）。
//!
//! - **持久化**：`dirs::config_dir()/tower-agent/mods` —— 用户显式保留的 mod；
//! - **临时**：`dirs::cache_dir()/tower-agent/mods` —— 会话内的工作副本，未持久化，
//!   由系统缓存在需要时清理。
//!
//! 每个 mod 存两个文件：`<name>.py`（源码）与 `<name>.json`（权限 + 依赖）。

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::manager::ModPermissions;

/// 落盘的 mod 元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModMeta {
    /// 权限。
    pub permissions: ModPermissions,
    /// 依赖的宿主能力（`tool.<name>`）。
    #[serde(default)]
    pub requires: Vec<String>,
}

/// 一个从磁盘读回的 mod。
#[derive(Debug, Clone)]
pub struct StoredMod {
    /// mod 名。
    pub name: String,
    /// 源码。
    pub code: String,
    /// 元数据。
    pub meta: ModMeta,
}

/// mod 存储位置。
#[derive(Debug, Clone)]
pub struct ModStore {
    /// 持久化目录（`dirs::config_dir()`）。
    pub config: Option<PathBuf>,
    /// 临时目录（`dirs::cache_dir()`）。
    pub cache: Option<PathBuf>,
}

impl Default for ModStore {
    fn default() -> Self {
        Self::from_dirs()
    }
}

impl ModStore {
    /// 用 `dirs` 的平台目录构造（config = 持久化，cache = 临时）。
    #[must_use]
    pub fn from_dirs() -> Self {
        Self {
            config: dirs::config_dir().map(|dir| dir.join("tower-agent").join("mods")),
            cache: dirs::cache_dir().map(|dir| dir.join("tower-agent").join("mods")),
        }
    }

    /// 显式指定目录（测试用）。
    #[must_use]
    pub fn with_dirs(config: Option<PathBuf>, cache: Option<PathBuf>) -> Self {
        Self { config, cache }
    }

    /// 把源码 + 元数据写进某个目录（尽力而为，失败不致命）。
    fn write_to(dir: &Path, name: &str, code: &str, meta: &ModMeta) {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let _ = fs::write(dir.join(format!("{name}.py")), code);
        if let Ok(json) = serde_json::to_vec_pretty(meta) {
            let _ = fs::write(dir.join(format!("{name}.json")), json);
        }
    }

    /// 临时保存（cache 目录）：会话内的工作副本，不算「保存」。
    pub fn save_ephemeral(&self, name: &str, code: &str, meta: &ModMeta) {
        if let Some(cache) = &self.cache {
            Self::write_to(cache, name, code, meta);
        }
    }

    /// 持久化：把临时副本复制到 config 目录；若 cache 没有则直接写入 config。
    pub fn persist(&self, name: &str, code: &str, meta: &ModMeta) -> Result<(), String> {
        let Some(config) = &self.config else {
            return Err("no config directory available".to_string());
        };
        Self::write_to(config, name, code, meta);
        Ok(())
    }

    /// 丢弃：从 config 与 cache 两个目录删除。
    pub fn discard(&self, name: &str) {
        for dir in [&self.config, &self.cache].into_iter().flatten() {
            let _ = fs::remove_file(dir.join(format!("{name}.py")));
            let _ = fs::remove_file(dir.join(format!("{name}.json")));
        }
    }

    /// 删除持久化副本（保留会话内的实例）。
    pub fn unpersist(&self, name: &str) {
        if let Some(dir) = &self.config {
            let _ = fs::remove_file(dir.join(format!("{name}.py")));
            let _ = fs::remove_file(dir.join(format!("{name}.json")));
        }
    }

    /// 读回持久化的 mod（字典序）。
    #[must_use]
    pub fn load_persisted(&self) -> Vec<StoredMod> {
        let Some(dir) = &self.config else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut mods: Vec<StoredMod> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("py") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.to_string();
                let code = fs::read_to_string(&path).ok()?;
                let meta = fs::read(dir.join(format!("{name}.json")))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default();
                Some(StoredMod { name, code, meta })
            })
            .collect();
        mods.sort_by(|left, right| left.name.cmp(&right.name));
        mods
    }
}
