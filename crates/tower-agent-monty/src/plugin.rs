//! 一个 wasm 沙箱 monty 插件：把沙箱实例 + 脚本 + 宿主接口 + 生命周期打包在一起。
//!
//! 执行全部发生在内嵌的 wasm 组件里（见 [`crate::sandbox`]）：脚本里未定义的函数调用
//! 会让组件返回一个 `function-call` 进度，宿主求值后（异步）恢复它，直到脚本跑完。

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tower_agent_plugins::{Lifecycle, PluginError, PluginManifest, PluginState};

use crate::{
    MontyHost,
    host::HostCall,
    python,
    sandbox::{Progress, Sandbox},
};

/// 一个内嵌 wasm 沙箱的 monty 插件。
pub struct MontyPlugin {
    manifest: PluginManifest,
    code: Mutex<String>,
    host: Arc<dyn MontyHost>,
    lifecycle: Lifecycle,
    sandbox: Mutex<Sandbox>,
}

impl MontyPlugin {
    /// 用初始脚本、宿主接口与一个沙箱实例构造（不执行；调用 [`MontyPlugin::activate`] 安装）。
    pub fn load(
        manifest: PluginManifest,
        code: impl Into<String>,
        host: Arc<dyn MontyHost>,
        sandbox: Sandbox,
    ) -> Result<Self, PluginError> {
        let name = manifest.name.clone();
        let plugin = Self {
            manifest,
            code: Mutex::new(code.into()),
            host,
            lifecycle: Lifecycle::new(name),
            sandbox: Mutex::new(sandbox),
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

    /// 在沙箱里驱动一段脚本直到完成，返回最终值的 JSON。
    async fn run_source(&self, source: &str, setup: bool) -> Result<String, PluginError> {
        let name = self.manifest.name.clone();
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| message_error(&name, "plugin is not started"))?;

        // 每次组件调用都是一段很短的同步临界区；宿主派发（异步）在锁外进行。
        let mut progress = { self.lock_sandbox()?.start(source)? };

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
                    // 锁外 await，避免在 await 点持有 `MutexGuard`。
                    let result = self.host.dispatch(host_call).await;
                    let call_id = call.call_id;
                    progress = match result {
                        Ok(value) => self
                            .lock_sandbox()?
                            .resume_function(call_id, &value.to_string())?,
                        Err(message) => self.lock_sandbox()?.reject_function(call_id, &message)?,
                    };
                }
                Progress::NameLookup(name_) => {
                    progress = self.lock_sandbox()?.resume_name(&name_)?;
                }
                Progress::OsCall(call_id) => {
                    progress = self.lock_sandbox()?.resume_os(call_id)?;
                }
                Progress::Error(message) => return Err(message_error(&name, message)),
            }
        }
    }

    fn lock_sandbox(&self) -> Result<std::sync::MutexGuard<'_, Sandbox>, PluginError> {
        self.sandbox
            .lock()
            .map_err(|_| message_error(&self.manifest.name, "sandbox mutex poisoned"))
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
