//! 一个 wasm 沙箱 monty 插件：脚本 + 宿主接口 + 生命周期 + 沙箱池。
//!
//! 每次运行从 [`SandboxPool`] 借一个沙箱实例，跑完归还；运行期间组件会以 `progress`
//! 的形式把外部函数调用 / OS 调用挂起给宿主，宿主算完后恢复，直到脚本跑完。

use std::sync::Arc;

use serde_json::Value;
use tower_agent_plugin_sdk::{Lifecycle, PluginError, PluginManifest, PluginState};

use crate::{
    MontyHost,
    host::HostCall,
    python,
    sandbox::{Progress, Sandbox, SandboxPool},
};

/// `exec` is a Python builtin, so the engine pre-binds the shared capability
/// name to a private host callback. Keep this protocol in sync with
/// `ModManager::dispatch`.
const CAPABILITY_PRELUDE: &str = "\
def exec(name, args, opts={}):
    return __codemode_exec(name, args, opts)
";

/// 一个内嵌 wasm 沙箱的 monty 插件。
pub struct MontyPlugin {
    manifest: PluginManifest,
    code: std::sync::Mutex<String>,
    host: Arc<dyn MontyHost>,
    lifecycle: Lifecycle,
    pool: Arc<SandboxPool>,
}

impl MontyPlugin {
    /// 用初始脚本、宿主接口与沙箱池构造（不执行；调用 [`MontyPlugin::activate`] 安装）。
    pub fn load(
        manifest: PluginManifest,
        code: impl Into<String>,
        host: Arc<dyn MontyHost>,
        pool: Arc<SandboxPool>,
    ) -> Result<Self, PluginError> {
        let name = manifest.name.clone();
        let plugin = Self {
            manifest,
            code: std::sync::Mutex::new(code.into()),
            host,
            lifecycle: Lifecycle::new(name),
            pool,
        };
        for state in [
            PluginState::Loaded,
            PluginState::Validated,
            PluginState::Resolved,
            PluginState::Started,
        ] {
            plugin.lifecycle.transition(state)?;
        }
        Ok(plugin)
    }

    /// 插件清单。
    #[must_use]
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// 生命周期句柄。
    #[must_use]
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }

    /// 当前脚本源码。
    #[must_use]
    pub fn code(&self) -> String {
        self.code.lock().expect("monty code poisoned").clone()
    }

    /// 安装阶段：跑一次脚本，让它注册工具 / 贡献 UI。
    pub async fn activate(&self) -> Result<(), PluginError> {
        let code = self.code();
        self.run_source(&code, true).await.map(|_| ())
    }

    /// 调用脚本里的 `function`，把 `args` 作为单个位置参数传入。
    pub async fn call_function(&self, function: &str, args: &Value) -> Result<Value, PluginError> {
        let code = self.code();
        let source = format!("{code}\n\n{function}({})", python::literal(args));
        let json = self.run_source(&source, false).await?;
        serde_json::from_str(&json).map_err(|error| {
            message_error(&self.manifest.name, format!("invalid result JSON: {error}"))
        })
    }

    /// Run the current source as a one-shot codemode program and return its
    /// final JSON value. Setup-only JIT registration calls are disabled.
    pub async fn run_codemode(&self, source: &str) -> Result<Value, PluginError> {
        let json = self.run_source(source, false).await?;
        serde_json::from_str(&json).map_err(|error| {
            message_error(
                &self.manifest.name,
                format!("invalid codemode result JSON: {error}"),
            )
        })
    }

    /// 原子替换脚本源码（下一次调用生效）。
    pub fn reload(&self, code: impl Into<String>) {
        *self.code.lock().expect("monty code poisoned") = code.into();
    }

    /// 试跑新脚本，成功才替换；失败保留旧版本并回报错误。
    pub async fn reload_checked(&self, code: impl Into<String>) -> Result<(), PluginError> {
        let code = code.into();
        self.run_source(&code, true).await?;
        *self.code.lock().expect("monty code poisoned") = code;
        Ok(())
    }

    /// 停止：卸载前必须调用。
    pub fn stop(&self) -> Result<(), PluginError> {
        self.lifecycle.stop()
    }

    /// 卸载：要求已 `stop` 且 in-flight 归零。
    pub fn unload(&self) -> Result<(), PluginError> {
        self.lifecycle.unload()
    }

    /// 借沙箱跑一段脚本；成功才把实例归还池子（出错/陷入 trap 的实例直接丢弃）。
    async fn run_source(&self, source: &str, setup: bool) -> Result<String, PluginError> {
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| message_error(&self.manifest.name, "plugin is not started"))?;
        let mut sandbox = self
            .pool
            .acquire()
            .map_err(crate::sandbox::to_plugin_error)?;
        let outcome = self.drive(&mut sandbox, source, setup).await;
        if outcome.is_ok() {
            self.pool.release(sandbox);
        }
        outcome
    }

    /// 驱动沙箱状态机直到完成。
    async fn drive(
        &self,
        sandbox: &mut Sandbox,
        source: &str,
        setup: bool,
    ) -> Result<String, PluginError> {
        let name = self.manifest.name.clone();
        let source = format!("{CAPABILITY_PRELUDE}\n{source}");
        let mut progress = sandbox.start(&source)?;

        loop {
            match progress {
                Progress::Complete(json) => return Ok(json),
                Progress::FunctionCall(call) => {
                    let host_call = HostCall {
                        function: call.function_name.clone(),
                        args: parse_args(&call.args_json),
                        kwargs: parse_kwargs(&call.kwargs_json),
                        setup,
                        plugin: name.clone(),
                    };
                    // 宿主派发是异步的；这里不持有任何锁。
                    let result = self.host.dispatch(host_call).await;
                    let call_id = call.call_id;
                    progress = match result {
                        Ok(value) => sandbox.resume_function(call_id, &value.to_string())?,
                        Err(message) => sandbox.reject_function(call_id, &message)?,
                    };
                }
                Progress::NameLookup(lookup) => {
                    progress = sandbox.resume_name(&lookup)?;
                }
                Progress::OsCall(call) => {
                    let result = self.host.handle_os_call(&call.payload_json);
                    progress = sandbox.resume_os(call.call_id, &result)?;
                }
                Progress::Error(message) => return Err(message_error(&name, message)),
            }
        }
    }
}

fn parse_args(json: &str) -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(json).unwrap_or_default()
}

fn parse_kwargs(json: &str) -> std::collections::BTreeMap<String, Value> {
    serde_json::from_str(json).unwrap_or_default()
}

fn message_error(plugin: &str, message: impl Into<String>) -> PluginError {
    PluginError::Load {
        plugin: plugin.to_string(),
        message: message.into(),
    }
}
