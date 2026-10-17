//! monty mods 的宿主能力接口。
//!
//! 脚本调用未定义的 `function(*args)` 时，monty 会挂起为 `TurnEvent::FunctionCall`；
//! 宿主把参数转成 JSON 交给 [`MontyHost::dispatch`]，再把结果送回沙箱。参数与返回值
//! 都是 JSON，因此一个接口就能承载「调工具 / 改 TUI / 读写状态」等所有能力。
//!
//! 共享 `exec` 协议：`exec(name: str, args: list[str], opts: object)` 返回
//! `{code: int, stdout: str, stderr: str}`。`opts` 支持 `stdin`（UTF-8 字符串）、
//! `timeout_ms`；stdout/stderr 也按 UTF-8 字符串返回（非法字节会被替换）。
//! `cwd` 字段预留给后续统一文件系统实现，当前会明确返回不支持。
//! Python 自带同名内建函数，因此引擎预载的 `exec` 包装会转发到内部 host call
//! `__codemode_exec`；脚本可见 API 仍是 `exec(...)`。

use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use serde_json::Value;

/// 一次来自沙箱的宿主调用。
#[derive(Debug, Clone)]
pub struct HostCall {
    /// 被调用的函数名（脚本里写的名字）。
    pub function: String,
    /// 位置参数。
    pub args: Vec<Value>,
    /// 关键字参数。
    pub kwargs: BTreeMap<String, Value>,
    /// 是否处于插件安装阶段（只有此时 `tool_register` 等生效）。
    pub setup: bool,
    /// 发起调用的插件名。
    pub plugin: String,
}

impl HostCall {
    /// 取第 `index` 个位置参数。
    #[must_use]
    pub fn arg(&self, index: usize) -> Option<&Value> {
        self.args.get(index)
    }

    /// 取名为 `name` 的关键字参数。
    #[must_use]
    pub fn kwarg(&self, name: &str) -> Option<&Value> {
        self.kwargs.get(name)
    }

    /// 位置参数或同名关键字参数。
    #[must_use]
    pub fn positional_or_kwarg(&self, index: usize, name: &str) -> Option<&Value> {
        self.arg(index).or_else(|| self.kwarg(name))
    }
}

/// mods 的宿主能力派发器。
///
/// 实现方负责解析函数名、检查权限并执行；返回 `Err` 会让沙箱侧抛出
/// `RuntimeError`（带上错误文本），从而把失败暴露给 LLM 生成的代码。
pub trait MontyHost: Send + Sync + 'static {
    /// 派发一次宿主调用。
    fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>>;

    /// 服务一次沙箱 OS 调用（文件系统等）。`call_json` 是序列化的 `OsFunctionCall`，
    /// 返回值是 `wire` 模块定义的三种结果之一。默认不处理。
    fn handle_os_call(&self, call_json: &str) -> String {
        let _ = call_json;
        crate::wire::NOT_HANDLED.to_string()
    }
}
