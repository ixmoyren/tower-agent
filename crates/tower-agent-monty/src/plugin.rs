//! 一个 monty 热重载插件：把沙箱脚本 + 宿主接口 + 生命周期打包在一起。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use monty_pool::{Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync};
use monty_types::{ExcType, MontyException, MontyObject, NameLookupResult, NamedValues};
use serde_json::Value;
use tower_agent_plugins::{Lifecycle, PluginError, PluginManifest, PluginState};

use crate::{MontyHost, host::HostCall, json};

/// 一个基于 monty-pool 的热重载插件。
///
/// 每次调用都会把脚本源码 + 一段调用表达式喂给沙箱（子进程，崩溃隔离）；脚本里
/// 未定义的函数调用会经 [`MontyHost::dispatch`] 回调宿主。热更新用
/// [`MontyPlugin::reload_checked`] 先试跑、成功后原子替换源码。
pub struct MontyPlugin {
    manifest: PluginManifest,
    pool: Pool,
    code: Mutex<String>,
    host: Arc<dyn MontyHost>,
    lifecycle: Lifecycle,
}

impl MontyPlugin {
    /// 用 worker 二进制路径与初始脚本构造（不执行；调用 [`MontyPlugin::activate`] 安装）。
    pub async fn load(
        manifest: PluginManifest,
        code: impl Into<String>,
        worker: impl Into<PathBuf>,
        host: Arc<dyn MontyHost>,
    ) -> Result<Self, PluginError> {
        let name = manifest.name.clone();
        let config = PoolConfig::subprocess(worker.into());
        let pool = Pool::new(config)
            .await
            .map_err(|error| load_error(&name, error))?;
        let plugin = Self {
            manifest,
            pool,
            code: Mutex::new(code.into()),
            host,
            lifecycle: Lifecycle::new(name.clone()),
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
    ///
    /// 此时 `tool_register` 等安装期宿主调用生效。
    pub async fn activate(&self) -> Result<(), PluginError> {
        let code = self.code();
        self.run_source(&code, true).await.map(|_| ())
    }

    /// 调用脚本里的 `function`，把 `args` 作为单个位置参数传入。
    ///
    /// 返回值是函数结果的 JSON 视图。
    pub async fn call_function(&self, function: &str, args: &Value) -> Result<Value, PluginError> {
        let code = self.code();
        let source = format!("{code}\n\n{function}({})", json::python_literal(args));
        let value = self.run_source(&source, false).await?;
        Ok(json::monty_to_json(&value))
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

    /// 在一个 checkout 会话里跑一段脚本，驱动 FunctionCall 挂起直到完成。
    async fn run_source(&self, source: &str, setup: bool) -> Result<MontyObject, PluginError> {
        let name = self.manifest.name.clone();
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| PluginError::Load {
                plugin: name.clone(),
                message: "plugin is not started".into(),
            })?;
        let mut checkout = self
            .pool
            .checkout(&ReplConfig::default())
            .await
            .map_err(|error| load_error(&name, error))?;

        let mut on_print = on_print_sync(|_stream, _text| {});
        let mut event = checkout
            .feed(source, NamedValues::new(), Vec::new(), true, &mut on_print)
            .await
            .map_err(|error| load_error(&name, error))?;

        loop {
            match event {
                TurnEvent::Complete(value) => {
                    checkout
                        .finish()
                        .await
                        .map_err(|error| load_error(&name, error))?;
                    return Ok(value);
                }
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    ..
                } => {
                    let call = HostCall {
                        function: function_name,
                        args: args.args().map(json::object_ref_to_json).collect(),
                        kwargs: args
                            .kwargs()
                            .map(|(key, value)| {
                                let key =
                                    key.as_str().map_or_else(|| key.py_repr(), str::to_string);
                                (key, json::object_ref_to_json(value))
                            })
                            .collect(),
                        setup,
                        plugin: name.clone(),
                    };
                    let resume = match self.host.dispatch(call).await {
                        Ok(value) => ResumeValue::Return(json::json_to_monty(&value)),
                        Err(message) => ResumeValue::Error(MontyException::new(
                            ExcType::RuntimeError,
                            Some(message),
                        )),
                    };
                    event = checkout
                        .resume(resume, &mut on_print)
                        .await
                        .map_err(|error| load_error(&name, error))?;
                }
                // 未定义的名字：让沙箱抛 NameError，而不是中断整个插件。
                TurnEvent::NameLookup { .. } => {
                    event = checkout
                        .resume_name_lookup(NameLookupResult::Undefined, &mut on_print)
                        .await
                        .map_err(|error| load_error(&name, error))?;
                }
                // 文件系统等 OS 调用：v1 全部交给沙箱的默认行为（通常报错）。
                TurnEvent::OsCall { .. } => {
                    event = checkout
                        .resume(ResumeValue::NotHandled, &mut on_print)
                        .await
                        .map_err(|error| load_error(&name, error))?;
                }
                other => {
                    return Err(PluginError::Load {
                        plugin: name,
                        message: format!("unsupported monty suspension: {other:?}"),
                    });
                }
            }
        }
    }
}

fn load_error(plugin: &str, error: PoolError) -> PluginError {
    PluginError::Load {
        plugin: plugin.into(),
        message: error.to_string(),
    }
}
