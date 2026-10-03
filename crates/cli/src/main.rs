//! `cli` — tower-pi-agent 的 CLI 入口（对应 pi `packages/coding-agent`）。
//!
//! 目前仅是可执行文件骨架，打印已移植的内置工具；会话与交互式命令在后续移植步骤中落地。

fn main() {
    println!("cli: tools = {}", cli::tools::tool_names().join(", "));
}
