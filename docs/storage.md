# Storage plugins and switching

`tower-agent` registers available storage implementations through
`tower-agent-plugin-sdk`'s `storage` feature. Runtime has exactly one active
backend; all configuration, credentials, sessions, history, memory and scripts
are read from and written to that backend.

## Selection

Upper layers such as the CLI receive `StorageBackendRegistration` values from
the plugin assembly layer. If more than one backend is available and no active
backend is configured, the frontend must ask the user to choose one. The
selection belongs in a separate control configuration file, not in the storage
backend being selected.

Use `StorageManager::new(backends, active)` to enforce the single-active rule.
`CliStore::from_manager` and other hosts can consume the manager through the
generic `Storage` trait.

The CLI stores selection state in:

```text
~/.config/tower-agent/storage.toml
```

```toml
[storage]
active = "turso"
disabled = []
```

`TOWER_AGENT_STORAGE_CONTROL` overrides this path. The file is written with
`temp file → fsync → rename → directory fsync`; it never lives in a storage
backend, so switching backends cannot move the selection itself.

At startup, one enabled backend is selected automatically. With multiple
enabled backends, an interactive terminal prompts for a choice; a
non-interactive process must pass `--storage <name>` and otherwise fails with
the candidate list. Once a selection exists, `--storage` can only confirm it.
Changing an existing selection requires the explicit migration command below.

## CLI commands

```sh
tower-agent storage list
tower-agent storage status
tower-agent storage switch <name>
tower-agent storage disable <name>
tower-agent storage enable <name>
```

`switch` runs the migration in the command process; no daemon or separate
maintenance process or `tower-agent` restart is required. Reads and writes are
blocked by the manager during the exclusive snapshot copy, but the process
itself does not stop. The active backend cannot be disabled.

## Switching

`StorageManager::switch_to_with(target, persist_selection)` performs an
in-process migration:

1. Acquire the manager's exclusive lock. New storage reads and writes wait.
2. Wait for all in-flight storage operations and flush the source.
3. Require an empty target backend.
4. Export one complete snapshot, including session transcripts.
5. Import the snapshot into the target and flush it.
6. Export the target again and compare the normalized snapshots.
7. Persist the new backend selection.
8. Change the in-memory active backend.

There is no dual-write. If copying, verification, or selection persistence
fails, the active backend remains the source and staging data is cleared on a
best-effort basis.

The source backend is retained after a successful switch. It can later be
disabled with `StorageManager::set_backend_enabled(name, false)`; disabling the
active backend is rejected.

## Backend requirements

`StorageMaintenance` supplies the operations needed by switching:

- `flush`
- full snapshot export/import
- clear staging

The default snapshot implementation walks all six storage domains. Backends
only need to provide scope enumeration for config/scripts and their normal
store methods.
