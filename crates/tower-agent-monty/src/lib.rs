//! 基于 [monty](https://crates.io/crates/monty) 的热重载插件宿主（`docs/plugins.md` §4.3）。
//!
//! monty 是 Rust 写的 Python 子集沙箱；这里用 `monty-pool` 把解释器跑在**工作子进程**里
//! （崩溃隔离）：崩溃只杀掉 worker，池会替换它，宿主进程不受影响。
//!
//! - 宿主能力调用：脚本调用未定义的外部函数会挂起为 [`TurnEvent::FunctionCall`]，由
//!   [`MontyHost`] 回调求值后 `resume` 继续——这就是“宿主函数即能力调用”。
//! - 热重载：[`MontyPlugin::reload`] 原子替换脚本源码；[`MontyPlugin::reload_checked`]
//!   先试跑一次，成功才替换、失败保留旧版本并回报错误。
//!
//! 运行需要 `monty` worker 二进制（`cargo build -p monty-runtime` 或 PyPI 的
//! `pydantic-monty-runtime`），路径由构造参数给出。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use monty_pool::{Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync};
use monty_types::{MontyObject, NamedValues};
use tower_agent_plugins::{Lifecycle, PluginError, PluginManifest, PluginState};

/// 宿主能力回调：脚本调用未定义的 `function(*args)` 时触发。
pub trait MontyHost: Send + Sync + 'static {
    /// 求值一次外部函数调用，返回值会被送回沙箱。
    fn call(&self, function: &str, args: &[String]) -> String;
}

/// 一个 monty 热重载插件。
pub struct MontyPlugin {
    manifest: PluginManifest,
    pool: Pool,
    code: Mutex<String>,
    host: Arc<dyn MontyHost>,
    lifecycle: Lifecycle,
}

impl MontyPlugin {
    /// 用 worker 二进制路径与初始脚本构造（并试跑一次校验）。
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

    /// 跑一次：把 `input` 作为沙箱全局 `input` 注入当前脚本，返回最终表达式的值。
    pub async fn call(&self, input: &str) -> Result<String, PluginError> {
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| PluginError::Load {
                plugin: self.manifest.name.clone(),
                message: "plugin is not started".into(),
            })?;
        let code = self.code();
        self.run(&code, Some(input)).await
    }

    /// 原子替换脚本源码（下一次调用生效）。
    pub fn reload(&self, code: impl Into<String>) {
        *self.code.lock().expect("monty code poisoned") = code.into();
    }

    /// 试跑新脚本，成功才替换；失败保留旧版本并回报错误。
    pub async fn reload_checked(&self, code: impl Into<String>) -> Result<(), PluginError> {
        let code = code.into();
        self.run(&code, Some("")).await?;
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

    /// 在一个 checkout 会话里跑一段脚本，驱动 FunctionCall 挂起。
    async fn run(&self, code: &str, input: Option<&str>) -> Result<String, PluginError> {
        let name = self.manifest.name.clone();
        let mut checkout = self
            .pool
            .checkout(&ReplConfig::default())
            .await
            .map_err(|error| load_error(&name, error))?;

        let mut inputs = NamedValues::new();
        if let Some(input) = input {
            inputs.push("input", MontyObject::string(input));
        }

        let mut on_print = on_print_sync(|_stream, _text| {});
        let mut event = checkout
            .feed(code, inputs, Vec::new(), true, &mut on_print)
            .await
            .map_err(|error| load_error(&name, error))?;

        loop {
            match event {
                TurnEvent::Complete(value) => {
                    checkout
                        .finish()
                        .await
                        .map_err(|error| load_error(&name, error))?;
                    return Ok(value.to_string());
                }
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    ..
                } => {
                    let args: Vec<String> = args.args().map(|arg| arg.to_string()).collect();
                    let result = self.host.call(&function_name, &args);
                    event = checkout
                        .resume(
                            ResumeValue::Return(MontyObject::string(result)),
                            &mut on_print,
                        )
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

/// 便捷：用一段源码与 worker 路径构造（清单名固定为 `monty`）。
pub async fn load_plugin(
    code: impl Into<String>,
    worker: impl Into<PathBuf>,
    host: Arc<dyn MontyHost>,
) -> Result<MontyPlugin, PluginError> {
    let manifest = PluginManifest::new(
        "monty",
        semver::Version::new(1, 0, 0),
        tower_agent_plugins::PluginKind::Jit,
    );
    MontyPlugin::load(manifest, code, worker, host).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoHost;

    impl MontyHost for NoHost {
        fn call(&self, _function: &str, _args: &[String]) -> String {
            String::new()
        }
    }

    fn worker() -> Option<PathBuf> {
        std::env::var_os("TPA_MONTY_BIN").map(PathBuf::from)
    }

    #[tokio::test]
    async fn runs_a_script_when_the_worker_is_available() {
        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };
        let plugin = load_plugin("input.upper()", worker, Arc::new(NoHost))
            .await
            .expect("load monty plugin");
        assert_eq!(plugin.call("hi").await.expect("call"), "HI");
    }

    #[tokio::test]
    async fn host_functions_are_resolved_by_the_callback() {
        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };
        struct Greet;
        impl MontyHost for Greet {
            fn call(&self, function: &str, args: &[String]) -> String {
                format!("{function}:{}", args.join(","))
            }
        }
        let plugin = load_plugin("greet(input)", worker, Arc::new(Greet))
            .await
            .expect("load monty plugin");
        assert_eq!(plugin.call("world").await.expect("call"), "greet:world");
    }
}
