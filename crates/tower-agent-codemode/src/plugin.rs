//! codemode 的**静态插件**形态。
//!
//! 与 [`CodemodeTool`](crate::CodemodeTool) 的关系：工具是「能干什么」，插件是「怎么被宿主
//! 纳入统一解析」——名字与能力都标成 **protected**，于是动态（dylib）与 JIT（monty mods）
//! 插件拿同名来顶替能力时会被拒绝（JIT 与动态两条路都已按名字响亮报错）。

use std::{path::PathBuf, sync::Mutex};

use semver::Version;
use tower_agent_core::AgentTool;
use tower_agent_plugins::{
    CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin,
};

use crate::{CodemodeError, CodemodeTool, Mount, SessionRoot};

/// 插件名（受保护，不可被遮蔽）。
pub const PLUGIN_NAME: &str = "codemode";

/// codemode 静态插件：安装时把 `codemode` 工具登记进上下文。
pub struct CodemodePlugin {
    cwd: PathBuf,
    session: SessionRoot,
    mounts: Vec<Mount>,
    /// 编译好的工具（`CodemodeTool::new` 会同步编译两个 wasm 组件，很贵）。
    ///
    /// 注册表每次 `resolve` 都会调用 `install`，所以这里只编译一次、之后 clone；
    /// `AgentTool` 非 `Sync`，用 `Mutex` 包一层让它能进 `StaticPlugin`。
    tool: Mutex<Option<AgentTool>>,
}

impl CodemodePlugin {
    /// 用工作目录与会话根形态构造（默认临时会话根 = 会话即焚）。
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, session: SessionRoot) -> Self {
        Self {
            cwd: cwd.into(),
            session,
            mounts: Vec::new(),
            tool: Mutex::new(None),
        }
    }

    /// 追加额外挂载。
    #[must_use]
    pub fn with_mounts(mut self, mounts: Vec<Mount>) -> Self {
        self.mounts = mounts;
        self
    }
}

impl StaticPlugin for CodemodePlugin {
    fn manifest(&self) -> PluginManifest {
        let version = Version::new(1, 0, 0);
        PluginManifest::new(PLUGIN_NAME, version.clone(), PluginKind::Static)
            .providing(CapabilityId::new("tool", "codemode"), version)
            .protected()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        let mut cached = self.tool.lock().map_err(|_| PluginError::Install {
            plugin: PLUGIN_NAME.to_owned(),
            message: "codemode plugin mutex poisoned".to_owned(),
        })?;
        if cached.is_none() {
            let tool =
                CodemodeTool::for_workspace_with(&self.cwd, self.session.clone(), &self.mounts)
                    .map_err(|error: CodemodeError| PluginError::Install {
                        plugin: PLUGIN_NAME.to_owned(),
                        message: error.to_string(),
                    })?
                    .into_agent_tool();
            *cached = Some(tool);
        }
        let tool = cached.as_ref().ok_or_else(|| PluginError::Install {
            plugin: PLUGIN_NAME.to_owned(),
            message: "codemode tool missing after build".to_owned(),
        })?;
        context.register_tool(tool.clone());
        Ok(())
    }
}
