//! JIT 插件（Roto + cranelift）（`docs/plugins.md` 第 4.3 节）。
//!
//! 插件是一段 Roto 脚本，宿主用 [`roto::Runtime`] 编译成宿主内模块；脚本里调用
//! `host_call(capability, arg)` 即宿主能力调用（由 [`JitHost`] 回调）。热加载 =
//! 重新编译 → 原子替换当前模块；编译失败保留旧版本并回报错误。调用在发起时绑定版本：
//! in-flight 调用持有旧模块的 `Arc`，换绑后发起的新调用使用新模块。

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use roto::{FileTree, NoCtx, RotoString, Runtime, TypedFunc};

use crate::{Lifecycle, PluginError, PluginManifest, PluginState};

/// 脚本入口签名：`fn main(input: String) -> String`。
type EntryFn = fn(RotoString) -> RotoString;

/// 宿主能力回调：脚本调用 `host_call(capability, arg)` 时触发。
pub trait JitHost: Send + Sync + 'static {
    /// 中介调用 `capability`，返回结果。
    fn call(&self, capability: &str, arg: &str) -> std::string::String;
}

/// 已编译的脚本：`TypedFunc` 自带 JIT 模块数据，可安全跨线程调用。
struct Compiled {
    entry: TypedFunc<NoCtx, EntryFn>,
}

/// 一个 JIT 插件：编译 Roto 脚本、中介调用并支持热加载/回滚。
pub struct JitPlugin {
    manifest: PluginManifest,
    path: PathBuf,
    runtime: Runtime<NoCtx>,
    current: RwLock<Arc<Compiled>>,
    lifecycle: Lifecycle,
}

impl JitPlugin {
    /// 从 `.roto` 文件加载并编译。
    pub fn load(
        manifest: PluginManifest,
        path: impl AsRef<Path>,
        host: Arc<dyn JitHost>,
    ) -> Result<Self, PluginError> {
        let path = path.as_ref().to_path_buf();
        let runtime = build_runtime(host)?;
        let compiled = compile_path(&runtime, &path, &manifest.name)?;
        let lifecycle = Lifecycle::new(manifest.name.clone());
        for state in [
            PluginState::Loaded,
            PluginState::Validated,
            PluginState::Resolved,
            PluginState::Started,
        ] {
            lifecycle.transition(state)?;
        }
        Ok(Self {
            manifest,
            path,
            runtime,
            current: RwLock::new(Arc::new(compiled)),
            lifecycle,
        })
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

    /// 中介调用脚本入口（带 in-flight 记账）。
    pub fn call(&self, input: &str) -> Result<String, PluginError> {
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| PluginError::Load {
                plugin: self.manifest.name.clone(),
                message: "plugin is not started".into(),
            })?;
        let compiled = Arc::clone(&*self.current.read().expect("jit lock poisoned"));
        let output = compiled.entry.call(RotoString::from(input));
        Ok(String::from(output))
    }

    /// 热加载：重新编译文件并原子换绑；编译失败保留旧版本并回报错误。
    pub fn reload(&self) -> Result<(), PluginError> {
        let compiled = compile_path(&self.runtime, &self.path, &self.manifest.name)?;
        *self.current.write().expect("jit lock poisoned") = Arc::new(compiled);
        Ok(())
    }

    /// 用一段脚本源码热加载（cc `mods` 场景：用户/LLM 直接写脚本字符串）。
    pub fn reload_source(&self, source: &str) -> Result<(), PluginError> {
        let compiled = compile_source(&self.runtime, source, &self.manifest.name)?;
        *self.current.write().expect("jit lock poisoned") = Arc::new(compiled);
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
}

/// 构造运行时，并注册 `host_call` 宿主能力。
fn build_runtime(host: Arc<dyn JitHost>) -> Result<Runtime<NoCtx>, PluginError> {
    let library = roto::library! {
        /// Call a host capability.
        let host_call = move |capability: RotoString, arg: RotoString| -> RotoString {
            RotoString::from(host.call(capability.as_ref(), arg.as_ref()))
        };
    };
    Runtime::from_lib(library).map_err(|error| PluginError::Load {
        plugin: "jit".into(),
        message: format!("failed to register host functions: {error}"),
    })
}

fn compile_path(
    runtime: &Runtime<NoCtx>,
    path: &Path,
    plugin: &str,
) -> Result<Compiled, PluginError> {
    let package = runtime.compile(path).map_err(|error| PluginError::Load {
        plugin: plugin.into(),
        message: error.to_string(),
    })?;
    finish_compile(package, plugin)
}

fn compile_source(
    runtime: &Runtime<NoCtx>,
    source: &str,
    plugin: &str,
) -> Result<Compiled, PluginError> {
    let package = FileTree::test_file("mods.roto", source, 0)
        .compile(runtime)
        .map_err(|error| PluginError::Load {
            plugin: plugin.into(),
            message: error.to_string(),
        })?;
    finish_compile(package, plugin)
}

fn finish_compile(
    mut package: roto::Package<NoCtx>,
    plugin: &str,
) -> Result<Compiled, PluginError> {
    let entry = package
        .get_function::<EntryFn>("main")
        .map_err(|error| PluginError::Load {
            plugin: plugin.into(),
            message: format!("missing `fn main(input: String) -> String`: {error}"),
        })?;
    Ok(Compiled { entry })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use semver::Version;

    use super::*;
    use crate::{CapabilityId, PluginKind};

    /// 宿主能力：`capability:arg`。
    struct EchoHost;

    impl JitHost for EchoHost {
        fn call(&self, capability: &str, arg: &str) -> std::string::String {
            format!("{capability}:{arg}")
        }
    }

    fn manifest() -> PluginManifest {
        PluginManifest::new("jit-demo", Version::new(1, 0, 0), PluginKind::Jit)
            .providing(CapabilityId::new("tool", "jit_demo"), Version::new(1, 0, 0))
    }

    fn temp_script(source: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tpa-jit-{}-{}.roto",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&path, source).expect("write script");
        path
    }

    #[test]
    fn compiles_calls_and_hot_swaps() {
        let path = temp_script("fn main(x: String) -> String { host_call(\"echo\", x) }");
        let plugin =
            JitPlugin::load(manifest(), &path, Arc::new(EchoHost)).expect("load jit plugin");
        assert_eq!(plugin.lifecycle().state(), PluginState::Started);
        assert_eq!(plugin.call("hi").expect("call"), "echo:hi");

        // 源码热加载：新模块接管后续调用。
        plugin
            .reload_source("fn main(x: String) -> String { host_call(\"echo2\", x) }")
            .expect("reload source");
        assert_eq!(plugin.call("hi").expect("call"), "echo2:hi");

        // 文件热加载。
        std::fs::write(
            &path,
            "fn main(x: String) -> String { host_call(\"file\", x) }",
        )
        .expect("rewrite script");
        plugin.reload().expect("reload file");
        assert_eq!(plugin.call("hi").expect("call"), "file:hi");
    }

    #[test]
    fn failed_reload_keeps_the_previous_module() {
        let path = temp_script("fn main(x: String) -> String { host_call(\"echo\", x) }");
        let plugin =
            JitPlugin::load(manifest(), &path, Arc::new(EchoHost)).expect("load jit plugin");

        assert!(
            plugin
                .reload_source("fn main(x: String) -> String { this is not roto }")
                .is_err(),
            "invalid script must fail to compile"
        );
        assert_eq!(
            plugin.call("hi").expect("call"),
            "echo:hi",
            "old module kept"
        );

        plugin.stop().expect("stop");
        plugin.unload().expect("unload");
    }
}
