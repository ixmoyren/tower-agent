# Tool description

在 wasm 沙箱中执行JavaScript（rquickjs）codemode 脚本，用于组合已授权的命令、文件、宿主工具和扩展能力。已授权能力：exec, fs, tools, register_tools, hooks, commands, ui, state。只能使用提示中明确列出的接口。

# Instructions

# Codemode

你在 wasm 沙箱中执行JavaScript（rquickjs）脚本。脚本通过宿主能力与外部世界交互，不能直接访问网络。

## 限制
- 单次执行时间上限：30000 ms
- 沙箱内存上限：128 MiB
- 网络：不可用

## 文件系统挂载
- `/work`：只读（workspace）
- `/output`：可读写（session output）

## 可用能力
未列出的 host function 不可调用；不要假设额外能力存在。

### exec
- `exec(name, args, opts)` 运行 coreutils-wasm 命令；返回 `code`、`stdout`、`stderr`。

### fs
- 文件操作只能访问上表挂载；写入范围由挂载模式决定。

### tools
- `tool_call(name, args)` 调用宿主工具；`tool_list()` 列出可用工具及其元数据。

### register_tools
- `tool_register(...)` 注册脚本工具；`tool_unregister(name)` 只注销脚本自己注册的工具。

### hooks
- `on("tool_call", handler)` 和 `on("tool_result", handler)` 可观察或调整工具调用。

### commands
- `register_command(name, handler)` 注册 agent 命令。

### ui
- `ui_status`、`ui_panel`、`ui_clear`、`ui_tool_render` 贡献前端无关的 UI。

### state
- `state_get(key)` / `state_set(key, value)` 读写脚本私有状态；`log(level, message)` 写日志。

## 语法示例

### exec
```js
const result = await exec("wc", ["-l", "/work/input.txt"]);
return result.stdout;
```

### fs
```js
const text = fs.readFile("/work/input.txt");
```

### tools
```js
const text = await tool_call("read", { path: "/work/input.txt" });
```

### register_tools
```js
function summarize(args) { return String(args.value); }
tool_register("summarize", "summarize", "Summarize a value", { type: "object" });
```

### hooks
```text
on("tool_call", handler)
on("tool_result", handler)
```

### commands
```text
register_command("name", handler)
```

### ui
```js
ui_status("job", "running", "blue", 1);
ui_panel("job", "Job", ["running"]);
```

### state
```js
await state_set("last", { value: 1 });
const state = await state_get("last");
log("info", "updated state");
return state;
```
