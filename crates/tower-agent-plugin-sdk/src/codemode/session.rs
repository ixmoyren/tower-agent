//! Engine-independent codemode session roots and mounts.

use std::path::{Path, PathBuf};

/// Session root shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRoot {
    /// A fresh temporary directory for one session.
    Ephemeral,
    /// A reusable directory shared across processes.
    Persistent(PathBuf),
}

/// One host-directory mount exposed at a guest path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Host-side directory.
    pub host: PathBuf,
    /// Guest path, for example `/work`.
    pub guest: String,
    /// Whether writes are rejected.
    pub read_only: bool,
}

impl Mount {
    /// Parse `<host>[:<guest>][:ro|rw]`; defaults to `/work` and read-only.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut parts = spec.split(':');
        let host = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| {
                format!("挂载规格 `{spec}` 缺 host（应为 `<dir>[:<guest>][:ro|rw]`）")
            })?;
        let mut guest = "/work".to_owned();
        let mut read_only = true;
        for part in parts {
            match part {
                "" => {}
                "ro" => read_only = true,
                "rw" => read_only = false,
                other if other.starts_with('/') => guest = other.to_owned(),
                other => {
                    return Err(format!(
                        "挂载规格 `{spec}` 里 `{other}` 不合法（guest 要以 / 开头，模式只能是 ro/rw）"
                    ));
                }
            }
        }
        Ok(Self {
            host: PathBuf::from(host),
            guest,
            read_only,
        })
    }

    /// Construct a read-only mount.
    #[must_use]
    pub fn read_only(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            read_only: true,
        }
    }

    /// Construct a writable mount.
    #[must_use]
    pub fn read_write(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            read_only: false,
        }
    }
}

/// Default directory for a named session.
#[must_use]
pub fn default_session_dir(name: &str) -> Option<PathBuf> {
    sessions_root().map(|root| root.join(name))
}

/// Base directory for named sessions.
#[must_use]
pub fn sessions_root() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| sessions_root_in(&dir))
}

/// Named-session directory beneath an explicit config root.
#[must_use]
pub fn sessions_root_in(config_root: &Path) -> PathBuf {
    config_root
        .join("tower-agent")
        .join("codemode")
        .join("sessions")
}

/// Whether a session name is exactly one ordinary path component.
#[must_use]
pub fn session_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && Path::new(name).components().count() == 1
}

/// List named-session directories in stable name order.
#[must_use]
pub fn list_sessions_in(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path())
        })
        .collect();
    sessions.sort();
    sessions
}

/// Delete one named session and its artifacts.
pub fn clear_session_in(root: &Path, name: &str) -> std::io::Result<bool> {
    if !session_name_is_valid(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("会话名 `{name}` 不合法（只能是一层普通名字）"),
        ));
    }
    let dir = root.join(name);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_spec_defaults_to_read_only_work() {
        let mount = Mount::parse("/tmp/workspace").expect("mount");
        assert_eq!(mount.host, PathBuf::from("/tmp/workspace"));
        assert_eq!(mount.guest, "/work");
        assert!(mount.read_only);
        assert!(!Mount::parse("/tmp:/data:rw").expect("mount").read_only);
    }

    #[test]
    fn session_names_cannot_escape_root() {
        assert!(session_name_is_valid("demo"));
        assert!(!session_name_is_valid(".."));
        assert!(!session_name_is_valid("a/b"));
    }
}
