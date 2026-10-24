# 存储插件与切换

`tower-agent` 通过 `tower-agent-plugin-sdk` 的 `storage` feature 注册可用的存储实现。
运行时同一时间只能有一个活动后端；所有配置、凭据、会话、历史、记忆和脚本都从该后端
读取并写入该后端。

## 选择

CLI 等上层组件从插件装配层接收 `StorageBackendRegistration`。如果存在多个可用后端，
且尚未配置活动后端，前端必须要求用户选择一个后端。选择结果应保存在独立的控制配置
文件中，而不是保存在待选择的存储后端内。

使用 `StorageManager::new(backends, active)` 强制落实“单活动后端”规则。
`CliStore::from_manager` 和其他宿主可以通过通用 `Storage` trait 使用该管理器。

CLI 将选择状态保存在：

```text
~/.config/tower-agent/storage.toml
```

```toml
[storage]
active = "turso"
disabled = []
```

环境变量 `TOWER_AGENT_STORAGE_CONTROL` 可以覆盖此路径。文件按
`临时文件 → fsync → rename → 目录 fsync` 的顺序写入；选择状态从不存放在存储后端中，
因此切换后端时不会连同选择状态一起迁移。

启动时，如果只有一个已启用后端，系统会自动选择它。如果存在多个已启用后端，交互式
终端会提示用户选择；非交互式进程必须通过 `--storage <name>` 指定后端，否则会以候选
后端列表作为错误信息退出。选择已经存在时，`--storage` 只能用于确认该选择。更改现有
选择必须执行下文的显式迁移命令。

## CLI 命令

```sh
tower-agent storage list
tower-agent storage status
tower-agent storage switch <name>
tower-agent storage disable <name>
tower-agent storage enable <name>
```

`switch` 会在命令进程中执行迁移，不需要守护进程、独立维护进程，也不需要重启
`tower-agent`。独占复制快照期间，管理器会阻塞存储读写，但进程本身不会停止。活动后端
不能被禁用。

## 切换流程

`StorageManager::switch_to_with(target, persist_selection)` 在同一进程内执行迁移：

1. 获取管理器的独占锁，新的存储读写开始等待。
2. 等待所有在途存储操作完成，并刷新源后端。
3. 要求目标后端为空。
4. 导出一份包含会话 transcript 的完整快照。
5. 将快照导入目标后端并刷新。
6. 再次导出目标后端，并比较标准化后的快照。
7. 持久化新的后端选择。
8. 切换内存中的活动后端。

迁移不做双写。如果复制、校验或选择状态持久化失败，活动后端仍保持为源后端，并尽力
清理暂存数据。

切换成功后，源后端仍然保留。之后可以通过
`StorageManager::set_backend_enabled(name, false)` 禁用它；禁用活动后端会被拒绝。

## 后端要求

`StorageMaintenance` 提供切换流程所需的操作：

- `flush`
- 完整快照导出与导入
- 清理暂存数据

默认快照实现会遍历全部六个存储域。后端只需为 config/scripts 提供作用域枚举，并实现
其常规存储方法。
