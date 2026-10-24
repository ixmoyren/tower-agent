//! Explicit codemode script export/import.
//!
//! Storage remains the source of truth. This module only projects scripts into one
//! reviewable file per script and restores them through [`ScriptStore`].

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_agent_plugin_sdk::codemode::{ScriptLanguage, ScriptPermissions};
use tower_agent_plugin_sdk::storage::{ConfigScope, ScriptRecord, ScriptStore};

const FORMAT_VERSION: u32 = 1;
const MAGIC_LINE: &str = "--- tower-agent-plugin-rquickjs v1";
const FILE_EXTENSION: &str = "codemode";

#[derive(Debug, Serialize, Deserialize)]
struct ScriptFileHeader {
    version: u32,
    id: String,
    name: String,
    language: ScriptLanguage,
    scope: ConfigScope,
    enabled: bool,
    permissions: ScriptPermissions,
    #[serde(default)]
    metadata: Value,
    created_at: i64,
    updated_at: i64,
    source_hash: String,
}

/// Export one scope into `dir`, replacing stale files created by a previous export.
///
/// The conflict policy is not invoked here: every storage record gets one deterministic
/// file name derived from its script name.
pub async fn export_scripts(
    store: &dyn ScriptStore,
    scope: &ConfigScope,
    dir: &Path,
) -> Result<usize, String> {
    let records = store
        .list_scripts(scope)
        .await
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(dir)
        .map_err(|error| format!("无法创建导出目录 `{}`：{error}", dir.display()))?;

    let mut exported = BTreeSet::new();
    for record in &records {
        let name = export_file_name(record);
        let path = dir.join(&name);
        fs::write(&path, render_script_file(record)?)
            .map_err(|error| format!("无法写入导出文件 `{}`：{error}", path.display()))?;
        exported.insert(name);
    }
    prune_stale_exports(dir, &exported)?;
    Ok(records.len())
}

/// Import every `*.codemode` file in `dir`. Same-scope/name records are overwritten.
///
/// When `scope_override` is set, all files are restored into that scope. Otherwise each
/// file is restored into the scope stored in its metadata header.
pub async fn import_scripts(
    store: &dyn ScriptStore,
    dir: &Path,
    scope_override: Option<&ConfigScope>,
) -> Result<usize, String> {
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("无法读取导入目录 `{}`：{error}", dir.display()))?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(FILE_EXTENSION))
        .collect::<Vec<_>>();
    paths.sort();

    let mut records = BTreeMap::new();
    for path in paths {
        let (mut header, source) = read_script_file(&path)?;
        if let Some(scope) = scope_override {
            header.scope = scope.clone();
        }
        let record = record_from_file(header, source)?;
        let key = (scope_key(&record.scope), record.name.clone());
        if let Some(previous) = records.insert(key, record) {
            return Err(format!(
                "导入目录中有重复脚本 `{}`（scope {}）",
                previous.name,
                scope_key(&previous.scope)
            ));
        }
    }

    let count = records.len();
    for record in records.into_values() {
        store
            .upsert_script(record)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn render_script_file(record: &ScriptRecord) -> Result<String, String> {
    let header = ScriptFileHeader {
        version: FORMAT_VERSION,
        id: record.id.clone(),
        name: record.name.clone(),
        language: ScriptLanguage::parse(&record.language)
            .ok_or_else(|| format!("脚本 `{}` 使用未知语言 `{}`", record.name, record.language))?,
        scope: record.scope.clone(),
        enabled: record.enabled,
        permissions: serde_json::from_value(record.permissions.clone())
            .map_err(|error| format!("脚本 `{}` 的权限元数据无效：{error}", record.name))?,
        metadata: record.metadata.clone(),
        created_at: record.created_at,
        updated_at: record.updated_at,
        source_hash: script_source_hash(&record.source),
    };
    let header = serde_json::to_string_pretty(&header)
        .map_err(|error| format!("无法序列化脚本 `{}` 的元数据：{error}", record.name))?;
    Ok(format!("{MAGIC_LINE}\n{header}\n---\n{}", record.source))
}

fn read_script_file(path: &Path) -> Result<(ScriptFileHeader, String), String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("无法读取脚本文件 `{}`：{error}", path.display()))?;
    let (first, rest) = content
        .split_once('\n')
        .ok_or_else(|| format!("脚本文件 `{}` 缺少 `{MAGIC_LINE}` 头", path.display()))?;
    if first.trim_end_matches('\r') != MAGIC_LINE {
        return Err(format!(
            "脚本文件 `{}` 缺少 `{MAGIC_LINE}` 头",
            path.display()
        ));
    }
    let (header, source) = split_header(rest)
        .ok_or_else(|| format!("脚本文件 `{}` 缺少元数据结束标记 `---`", path.display()))?;
    let header: ScriptFileHeader = serde_json::from_str(header).map_err(|error| {
        format!(
            "脚本文件 `{}` 的元数据不是有效 JSON：{error}",
            path.display()
        )
    })?;
    if header.version != FORMAT_VERSION {
        return Err(format!(
            "脚本文件 `{}` 使用不支持的格式版本 {}",
            path.display(),
            header.version
        ));
    }
    if header.name.trim().is_empty() {
        return Err(format!("脚本文件 `{}` 的名字为空", path.display()));
    }
    let actual_hash = script_source_hash(source);
    if header.source_hash != actual_hash {
        return Err(format!(
            "脚本文件 `{}` 的源码哈希不匹配（头部 {}，实际 {actual_hash}）",
            path.display(),
            header.source_hash
        ));
    }
    Ok((header, source.to_owned()))
}

fn split_header(rest: &str) -> Option<(&str, &str)> {
    if let Some((header, source)) = rest.split_once("\n---\n") {
        return Some((header, source));
    }
    rest.split_once("\r\n---\r\n")
}

fn record_from_file(header: ScriptFileHeader, source: String) -> Result<ScriptRecord, String> {
    if header.id.trim().is_empty() {
        return Err(format!("脚本 `{}` 的 id 为空", header.name));
    }
    let permissions = serde_json::to_value(header.permissions)
        .map_err(|error| format!("无法序列化脚本 `{}` 的权限：{error}", header.name))?;
    Ok(ScriptRecord {
        source_hash: script_source_hash(&source),
        id: header.id,
        name: header.name,
        language: header.language.as_str().to_owned(),
        scope: header.scope,
        source,
        enabled: header.enabled,
        permissions,
        metadata: header.metadata,
        created_at: header.created_at,
        updated_at: header.updated_at,
    })
}

fn export_file_name(record: &ScriptRecord) -> String {
    let mut stem = record
        .name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    if stem.is_empty() {
        stem.push_str("script");
    }
    format!(
        "{stem}-{:016x}.{FILE_EXTENSION}",
        fnv1a(record.name.as_bytes())
    )
}

fn prune_stale_exports(dir: &Path, exported: &BTreeSet<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("无法扫描导出目录 `{}`：{error}", dir.display()))?;
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(FILE_EXTENSION) {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if exported.contains(file_name) {
            continue;
        }
        let is_our_file = fs::read_to_string(&path)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .next()
                    .map(|line| line.trim_end_matches('\r') == MAGIC_LINE)
            })
            .unwrap_or(false);
        if is_our_file {
            fs::remove_file(&path)
                .map_err(|error| format!("无法清理旧导出文件 `{}`：{error}", path.display()))?;
        }
    }
    Ok(())
}

fn scope_key(scope: &ConfigScope) -> String {
    let (kind, id) = scope.storage_parts();
    format!("{kind}:{id}")
}

fn script_source_hash(source: &str) -> String {
    format!("{:016x}", fnv1a(source.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ScriptRecord {
        ScriptRecord {
            id: "script-1".to_owned(),
            name: "../unsafe/name".to_owned(),
            language: "monty".to_owned(),
            scope: ConfigScope::project("/work/a"),
            source: "value = 1\n".to_owned(),
            enabled: true,
            permissions: serde_json::json!({
                "exec": false,
                "fs": true,
                "tools": true,
                "register_tools": true,
                "hooks": true,
                "commands": true,
                "ui": true,
                "state": true,
            }),
            metadata: serde_json::json!({ "requires": ["tool.read"] }),
            created_at: 10,
            updated_at: 20,
            source_hash: script_source_hash("value = 1\n"),
        }
    }

    #[test]
    fn script_file_round_trips_metadata_and_source() {
        let record = record();
        let rendered = render_script_file(&record).expect("render");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("script.codemode");
        fs::write(&path, rendered).expect("write");

        let (header, source) = read_script_file(&path).expect("parse");
        let restored = record_from_file(header, source).expect("record");
        assert_eq!(restored, record);
    }

    #[test]
    fn script_file_name_is_safe_and_deterministic() {
        let name = export_file_name(&record());
        assert!(name.ends_with(".codemode"));
        assert!(!name.contains('/'));
        assert_eq!(name, export_file_name(&record()));
    }

    #[test]
    fn source_hash_mismatch_is_rejected() {
        let mut rendered = render_script_file(&record()).expect("render");
        rendered.push_str("tampered = true\n");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("script.codemode");
        fs::write(&path, rendered).expect("write");
        assert!(
            read_script_file(&path)
                .expect_err("hash mismatch")
                .contains("哈希不匹配")
        );
    }

    #[tokio::test]
    async fn import_overwrites_same_scope_and_name() {
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("storage");
        let store = std::sync::Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let script_store = store.script_store();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(export_file_name(&record()));

        fs::write(&path, render_script_file(&record()).expect("first file")).expect("write");
        import_scripts(script_store.as_ref(), dir.path(), None)
            .await
            .expect("first import");

        let mut replacement = record();
        replacement.source = "value = 2\n".to_owned();
        replacement.updated_at = 30;
        replacement.source_hash = script_source_hash(&replacement.source);
        fs::write(
            &path,
            render_script_file(&replacement).expect("replacement file"),
        )
        .expect("overwrite file");
        import_scripts(script_store.as_ref(), dir.path(), None)
            .await
            .expect("second import");

        let stored = script_store
            .get_script(&replacement.scope, &replacement.name)
            .await
            .expect("read")
            .expect("stored");
        assert_eq!(stored.source, "value = 2\n");
        assert_eq!(stored.updated_at, 30);
    }

    #[tokio::test]
    async fn export_prunes_only_owned_stale_script_files() {
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("storage");
        let store = std::sync::Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let dir = tempfile::tempdir().expect("temp dir");
        let owned_stale = dir.path().join("stale.codemode");
        let unrelated = dir.path().join("notes.codemode");
        fs::write(&owned_stale, format!("{MAGIC_LINE}\n{{}}\n---\nstale\n")).expect("owned stale");
        fs::write(&unrelated, "human notes\n").expect("unrelated");

        export_scripts(
            store.script_store().as_ref(),
            &ConfigScope::project("/work/a"),
            dir.path(),
        )
        .await
        .expect("empty export");

        assert!(!owned_stale.exists());
        assert!(unrelated.exists());
    }
}
