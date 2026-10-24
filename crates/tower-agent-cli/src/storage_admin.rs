//! CLI-side storage backend discovery, status, and in-process migration.
//!
//! The selection is stored outside every backend in `storage.toml`. A switch
//! builds one manager over all registered backends, copies a complete snapshot,
//! verifies it, persists the new selection, and only then changes the active
//! in-memory route.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use tokio::runtime::Runtime;
use tower_agent_plugin_sdk::storage::{StorageBackendRegistration, StorageError, StorageManager};
use tower_agent_plugin_turso::TursoStorage;

use crate::{
    args::StorageAction,
    store::{
        StorageControl, load_storage_control, save_storage_control, storage_control_path,
        storage_path,
    },
};

/// Run a storage management action with the CLI's default backend set.
pub fn run(runtime: &Runtime, action: &StorageAction) -> Result<String, StorageError> {
    let backends = default_backends(runtime)?;
    run_with_backends(runtime, backends, storage_control_path(), action)
}

/// Run a storage management action against explicitly discovered backends.
pub fn run_with_backends(
    runtime: &Runtime,
    backends: Vec<StorageBackendRegistration>,
    control_path: impl AsRef<Path>,
    action: &StorageAction,
) -> Result<String, StorageError> {
    let control_path = control_path.as_ref();
    let control = load_storage_control(control_path)?;
    let names = backend_names(&backends)?;

    match action {
        StorageAction::List => Ok(render_list(&names, &control)),
        StorageAction::Status => Ok(render_status(&names, &control)),
        StorageAction::Switch { name } => switch(runtime, backends, control_path, &control, name),
        StorageAction::Disable { name } => {
            set_enabled(runtime, backends, control_path, &control, name, false)
        }
        StorageAction::Enable { name } => {
            set_enabled(runtime, backends, control_path, &control, name, true)
        }
    }
}

/// Open the default Turso backend and wrap it as a storage plugin registration.
pub fn default_backends(
    runtime: &Runtime,
) -> Result<Vec<StorageBackendRegistration>, StorageError> {
    let backend = runtime.block_on(TursoStorage::open(storage_path()))?;
    Ok(vec![backend.plugin().registration()])
}

fn switch(
    runtime: &Runtime,
    backends: Vec<StorageBackendRegistration>,
    control_path: &Path,
    control: &StorageControl,
    target: &str,
) -> Result<String, StorageError> {
    let names = backend_names(&backends)?;
    if !names.contains(target) {
        return Err(StorageError::Unavailable(target.to_owned()));
    }
    let Some(active) = control.active.as_deref() else {
        return Err(StorageError::Selection(
            "no active storage backend is configured".to_owned(),
        ));
    };
    if active == target {
        return Ok(format!("storage backend 已经是 `{target}`，无需切换。"));
    }
    if control.disabled.contains(target) {
        return Err(StorageError::Unavailable(format!(
            "backend `{target}` is disabled; run `tower-agent storage enable {target}` first"
        )));
    }

    let manager = Arc::new(StorageManager::new(backends, active.to_owned())?);
    runtime.block_on(async {
        for disabled in control.disabled.intersection(&names) {
            manager.set_backend_enabled(disabled, false).await?;
        }
        let mut next = control.clone();
        manager
            .switch_to_with(target, move |selected| {
                next.active = Some(selected.to_owned());
                save_storage_control(control_path, &next)
            })
            .await
    })?;
    Ok(format!(
        "已将 storage 从 `{active}` 迁移并切换到 `{target}`。"
    ))
}

fn set_enabled(
    runtime: &Runtime,
    backends: Vec<StorageBackendRegistration>,
    control_path: &Path,
    control: &StorageControl,
    target: &str,
    enabled: bool,
) -> Result<String, StorageError> {
    let names = backend_names(&backends)?;
    if !names.contains(target) {
        return Err(StorageError::Unavailable(target.to_owned()));
    }
    if enabled && !control.disabled.contains(target) {
        return Ok(format!("storage backend `{target}` 已经是启用状态。"));
    }
    if !enabled && control.disabled.contains(target) {
        return Ok(format!("storage backend `{target}` 已经是停用状态。"));
    }

    let active = control.active.as_deref().ok_or_else(|| {
        StorageError::Selection("no active storage backend is configured".to_owned())
    })?;
    let manager = Arc::new(StorageManager::new(backends, active.to_owned())?);
    runtime.block_on(async {
        for disabled in control.disabled.intersection(&names) {
            manager.set_backend_enabled(disabled, false).await?;
        }
        manager.set_backend_enabled(target, enabled).await
    })?;

    let mut next = control.clone();
    if enabled {
        next.disabled.remove(target);
    } else {
        next.disabled.insert(target.to_owned());
    }
    save_storage_control(control_path, &next)?;
    if enabled {
        Ok(format!("已启用 storage backend `{target}`。"))
    } else {
        Ok(format!("已停用 storage backend `{target}`。"))
    }
}

fn backend_names(
    backends: &[StorageBackendRegistration],
) -> Result<BTreeSet<String>, StorageError> {
    let mut names = BTreeSet::new();
    for backend in backends {
        if !names.insert(backend.name.clone()) {
            return Err(StorageError::Migration(format!(
                "duplicate storage backend name `{}`",
                backend.name
            )));
        }
    }
    Ok(names)
}

fn render_list(names: &BTreeSet<String>, control: &StorageControl) -> String {
    let mut lines = vec!["NAME\tSTATUS".to_owned()];
    for name in names {
        let status = if control.active.as_deref() == Some(name) {
            "active"
        } else if control.disabled.contains(name) {
            "disabled"
        } else {
            "enabled"
        };
        lines.push(format!("{name}\t{status}"));
    }
    lines.join("\n")
}

fn render_status(names: &BTreeSet<String>, control: &StorageControl) -> String {
    match control.active.as_deref() {
        Some(active) if names.contains(active) => format!("active\t{active}"),
        Some(active) => format!("active\t{active} (unavailable)"),
        None => {
            let enabled = names.difference(&control.disabled).collect::<Vec<_>>();
            match enabled.as_slice() {
                [only] => format!("active\t{only} (implicit)"),
                [] => "active\t<none>".to_owned(),
                _ => "active\t<none> (multiple candidates)".to_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;
    use tempfile::TempDir;
    use tower_agent_plugin_sdk::storage::{ConfigScope, StoragePlugin, StorageSnapshot};

    use super::*;

    fn backend(
        name: &str,
        storage: Arc<dyn tower_agent_plugin_sdk::storage::Storage>,
    ) -> StorageBackendRegistration {
        StoragePlugin::new(name, Version::new(1, 0, 0), storage).registration()
    }

    fn open_backend(runtime: &Runtime) -> Arc<dyn tower_agent_plugin_sdk::storage::Storage> {
        let storage = runtime
            .block_on(TursoStorage::in_memory())
            .expect("open in-memory backend");
        Arc::new(storage)
    }

    fn control_path(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("storage.toml")
    }

    #[test]
    fn switch_migrates_and_persists_selection() {
        let runtime = Runtime::new().expect("runtime");
        let source = open_backend(&runtime);
        let target = open_backend(&runtime);
        runtime
            .block_on(source.set_config(&ConfigScope::User, "theme", serde_json::json!("dark")))
            .expect("seed source");
        runtime.block_on(source.flush()).expect("flush source");

        let dir = TempDir::new().expect("temp dir");
        let control_path = control_path(&dir);
        save_storage_control(
            &control_path,
            &StorageControl {
                active: Some("source".to_owned()),
                ..StorageControl::default()
            },
        )
        .expect("save control");

        let result = run_with_backends(
            &runtime,
            vec![
                backend("source", Arc::clone(&source)),
                backend("target", Arc::clone(&target)),
            ],
            &control_path,
            &StorageAction::Switch {
                name: "target".to_owned(),
            },
        )
        .expect("switch");
        assert!(result.contains("target"));
        assert_eq!(
            load_storage_control(&control_path)
                .expect("load control")
                .active
                .as_deref(),
            Some("target")
        );
        assert_eq!(
            runtime
                .block_on(target.get_config(&ConfigScope::User, "theme"))
                .expect("read target"),
            Some(serde_json::json!("dark"))
        );
    }

    #[test]
    fn failed_switch_keeps_old_selection_and_source_data() {
        let runtime = Runtime::new().expect("runtime");
        let source = open_backend(&runtime);
        let target = open_backend(&runtime);
        runtime
            .block_on(source.set_config(&ConfigScope::User, "theme", serde_json::json!("dark")))
            .expect("seed source");
        runtime
            .block_on(target.set_config(&ConfigScope::User, "existing", serde_json::json!(true)))
            .expect("seed target");
        runtime.block_on(source.flush()).expect("flush source");
        runtime.block_on(target.flush()).expect("flush target");

        let dir = TempDir::new().expect("temp dir");
        let control_path = control_path(&dir);
        save_storage_control(
            &control_path,
            &StorageControl {
                active: Some("source".to_owned()),
                ..StorageControl::default()
            },
        )
        .expect("save control");

        let error = run_with_backends(
            &runtime,
            vec![
                backend("source", Arc::clone(&source)),
                backend("target", Arc::clone(&target)),
            ],
            &control_path,
            &StorageAction::Switch {
                name: "target".to_owned(),
            },
        )
        .expect_err("non-empty target must fail");
        assert!(matches!(error, StorageError::Migration(_)));
        assert_eq!(
            load_storage_control(&control_path)
                .expect("load control")
                .active
                .as_deref(),
            Some("source")
        );
        assert_eq!(
            runtime
                .block_on(source.get_config(&ConfigScope::User, "theme"))
                .expect("read source"),
            Some(serde_json::json!("dark"))
        );
        let snapshot: StorageSnapshot = runtime
            .block_on(target.export_snapshot())
            .expect("target snapshot");
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn active_backend_cannot_be_disabled() {
        let runtime = Runtime::new().expect("runtime");
        let source = open_backend(&runtime);
        let other = open_backend(&runtime);
        let dir = TempDir::new().expect("temp dir");
        let control_path = control_path(&dir);
        save_storage_control(
            &control_path,
            &StorageControl {
                active: Some("source".to_owned()),
                ..StorageControl::default()
            },
        )
        .expect("save control");

        let error = run_with_backends(
            &runtime,
            vec![backend("source", source), backend("other", other)],
            control_path,
            &StorageAction::Disable {
                name: "source".to_owned(),
            },
        )
        .expect_err("active backend must not be disabled");
        assert!(matches!(error, StorageError::Unavailable(_)));
    }

    #[test]
    fn disable_and_enable_are_persisted() {
        let runtime = Runtime::new().expect("runtime");
        let source = open_backend(&runtime);
        let other = open_backend(&runtime);
        let dir = TempDir::new().expect("temp dir");
        let control_path = control_path(&dir);
        save_storage_control(
            &control_path,
            &StorageControl {
                active: Some("source".to_owned()),
                ..StorageControl::default()
            },
        )
        .expect("save control");

        run_with_backends(
            &runtime,
            vec![
                backend("source", Arc::clone(&source)),
                backend("other", Arc::clone(&other)),
            ],
            &control_path,
            &StorageAction::Disable {
                name: "other".to_owned(),
            },
        )
        .expect("disable");
        assert!(
            load_storage_control(&control_path)
                .expect("load control")
                .disabled
                .contains("other")
        );
        run_with_backends(
            &runtime,
            vec![backend("source", source), backend("other", other)],
            &control_path,
            &StorageAction::Enable {
                name: "other".to_owned(),
            },
        )
        .expect("enable");
        assert!(
            !load_storage_control(&control_path)
                .expect("load control")
                .disabled
                .contains("other")
        );
    }
}
