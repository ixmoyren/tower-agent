# Tool description

在 wasm 沙箱中执行JavaScript（rquickjs）codemode 脚本，用于组合已授权的命令、文件、宿主工具和扩展能力。已授权能力：fs。只能使用提示中明确列出的接口。

# Instructions

# Codemode

你在 wasm 沙箱中执行JavaScript（rquickjs）脚本。脚本通过宿主能力与外部世界交互，不能直接访问网络。

## 限制
- 单次执行时间上限：5000 ms
- 沙箱内存上限：64 MiB
- 网络：不可用

## 文件系统挂载
- `/work`：只读（workspace）
- `/output`：可读写（session output）

## 可用能力
未列出的 host function 不可调用；不要假设额外能力存在。

### fs
- 文件操作只能访问上表挂载；写入范围由挂载模式决定。

## 语法示例

### fs
```js
const text = fs.readFile("/work/input.txt");
```
