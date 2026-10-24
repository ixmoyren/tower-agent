# tower-agent-plugin-shell

`shell` 工具现在作为独立动态插件（`cdylib`）发布，不再编译进
`tower-agent-plugin-tools`。插件内部继续使用 brush，保持 bash 兼容和跨命令的
shell 状态。

## 构建

```sh
cargo build -p tower-agent-plugin-shell --release
```

产物名为 `libtower_agent_plugin_shell.so`（macOS 为 `.dylib`，Windows 为
`.dll`）。

## 安装

把产物复制到用户动态插件目录：

```sh
mkdir -p ~/.config/tower-agent/plugins
cp target/release/libtower_agent_plugin_shell.so \
  ~/.config/tower-agent/plugins/
```

宿主启动时会从 `dirs::config_dir()/tower-agent/plugins` 加载它，并注册
`tool.shell`。

如果没有找到这个动态库，`tower-agent` **不会退出**：其他工具继续可用，
TUI 会显示一条 notice，`--print` 模式会向 stderr 输出安装提示。
