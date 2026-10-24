//! 会话入口：把 agent、内置工具与（可选的）TUI 拼成一个可运行的会话。
//!
//! 一层的职责划分：
//!
//! - [`Session`] 通过 [`crate::stack`] 用 `ServiceBuilder` 装配出类型擦除的
//!   `BoxAgentService`（跑 turn）与 `BoxAgentControl`（订阅事件、切模型/工具、取消、重置），
//!   自身不直接持有 `Agent` 具体类型；内置工具由 `tower_agent_plugin` 装配；
//! - 事件通过 [`Session::subscribe`] 拿到的 `tower_agent_runtime::Subscription` 流出，
//!   既可以交给 [`tower_agent_tui::run_agent_tui`] 渲染，也可以用 [`run_print`] 打成纯文本（便于脚本与测试）；
//! - provider 由调用方注入（[`Session::new`] 的 `stream` 参数）；测试用确定性的离线
//!   provider [`tower_agent_provider_builtin::provider::faux::FauxProvider`]，二进制则接真实 provider。

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use tower::ServiceExt as _;
use tower_agent_codemode_core::{ScriptHookRegistry, ScriptLanguage, ScriptMetadata};
use tower_agent_core::UiState;
#[cfg(feature = "codemode-python")]
use tower_agent_monty::ModManager;
use tower_agent_plugin::PermissionPolicy;
#[cfg(feature = "codemode-js")]
use tower_agent_plugin::StaticPlugin as _;
use tower_agent_plugin_sdk::storage::{ConfigScope, ProviderCredential};

use crate::store::CliStore;
use tower_agent_provider::{
    BoxStreamService, ContentBlock, Message, Model, ModelThinkingLevel, ProviderRegistry,
    TextContent, UserContent, UserMessage,
};
use tower_agent_runtime::{
    AgentError, AgentEvent, AgentMessage, AgentOptions, AgentRequest, AgentState, AgentTool,
    BoxAgentControl, BoxAgentService, Compactor, Subscription,
};

/// 会话。
///
/// 工具目录的唯一来源是 mods 注册表（启用 mods 时）；`--tools` 等选项以 [`ToolFilter`]
/// 的形式作用在目录之上。
pub struct Session {
    // `BoxAgentService` 是 `Send` 但非 `Sync`；用 `Mutex` 包一层让 `Session` 可跨线程共享
    // （TUI 驱动任务把 `Arc<Session>` move 进 `tokio::spawn`）。
    service: Mutex<BoxAgentService>,
    control: BoxAgentControl,
    cwd: PathBuf,
    /// mods 未启用时的工具目录（内置工具）。
    base_tools: Mutex<Vec<AgentTool>>,
    /// codemode Python 宿主；解释器内嵌在 `tower-agent-monty` 中，默认启用。
    #[cfg(feature = "codemode-python")]
    mods: Option<Arc<ModManager>>,
    /// 工具过滤（`--tools` / `--exclude-tools` / `--no-tools`）。
    tool_filter: Mutex<ToolFilter>,
    /// codemode 的进程级默认引擎（脚本元数据可覆盖）。
    codemode_engine: Mutex<ScriptLanguage>,
    /// Runtime permission policy shared by both codemode engines.
    permission_policy: PermissionPolicy,
    /// Session-scoped script hooks shared by both codemode engines.
    script_hooks: Arc<ScriptHookRegistry>,
    /// 配置存储：`/login` `/model` `/thinking` 会把结果写回（对齐 pi 的 `auth.json` /
    /// `settings.json`）；未接入存储时为 `None`。
    store: Option<Arc<CliStore>>,
    /// transcript 持久化句柄（会话按项目归属）；未落盘时为 `None`。
    transcript: Option<Arc<crate::store::Transcript>>,
    /// 上下文压缩器（核心能力，由 agent 循环在每次请求前自动执行）；未启用时为 `None`。
    compactor: Option<Arc<Compactor>>,
    /// Provider plugins available to `/login` and `/model`.
    providers: Arc<ProviderRegistry>,
    /// Credentials known to this process, used by provider switches.
    provider_credentials: Mutex<BTreeMap<String, String>>,
}

/// 工具过滤：`none` 清空，`allow` 白名单（空 = 全放行），`exclude` 黑名单。
#[derive(Debug, Clone, Default)]
pub struct ToolFilter {
    /// `--no-tools` / `--no-builtin-tools`。
    pub none: bool,
    /// `--tools` 白名单。
    pub allow: Vec<String>,
    /// `--exclude-tools` 黑名单。
    pub exclude: Vec<String>,
}

/// Startup result for native dynamic plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicPluginLoad {
    loaded: Vec<String>,
    error: Option<String>,
}

impl DynamicPluginLoad {
    /// User-facing warning when the optional shell plugin is unavailable.
    ///
    /// A missing plugin never aborts startup: every static tool and codemode
    /// remains usable, while the caller decides whether to show this as a TUI
    /// notice or a stderr warning.
    #[must_use]
    pub fn shell_notice(&self) -> Option<String> {
        if self.loaded.iter().any(|name| name == "shell") {
            return None;
        }
        let directory = dirs::config_dir()
            .map(|dir| {
                dir.join("tower-agent")
                    .join("plugins")
                    .display()
                    .to_string()
            })
            .unwrap_or_else(|| "<系统的配置目录>/tower-agent/plugins".to_owned());
        let reason = self.error.as_deref().unwrap_or("未发现 shell 插件库");
        Some(format!(
            "shell 工具暂不可用：{reason}。其他工具仍可正常使用。\n\
             请构建 `tower-agent-plugin-shell` 并把动态库放到：{directory}\n\
             构建命令：cargo build -p tower-agent-plugin-shell --release"
        ))
    }
}

impl ToolFilter {
    /// 某个工具名是否通过过滤。
    #[must_use]
    pub fn allows(&self, name: &str) -> bool {
        if self.none {
            return false;
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|allowed| allowed == name) {
            return false;
        }
        !self.exclude.iter().any(|excluded| excluded == name)
    }
}

impl Session {
    /// 用工作目录和一个 provider 流服务构造会话，并注册全部内置工具。
    ///
    /// 默认把 provider 报出来的模型记进 transcript；[`Session::with_model`] 可以覆盖。
    pub fn new(cwd: impl AsRef<Path>, stream: BoxStreamService) -> Self {
        Self::with_model(cwd, stream, default_model())
    }

    /// 打开 codemode（`--codemode`）：构造工具并加进地基工具集，下一轮 turn 生效。
    ///
    /// 布局是「工作区只读挂到 `/work` + 会话根」；`session` 给了名字就把会话根**持久化**在
    /// `<config>/tower-agent/codemode/sessions/<name>`（跨进程复用），否则用临时目录（会话即焚）。
    ///
    /// 启用 mods 时把 codemode 作为**受保护静态插件**注册进统一注册表（名字 `codemode`、
    /// 能力 `tool.codemode` 因此进 `protected_*`，动态 / JIT 插件不能顶替）；未启用 mods
    /// 时退化成直接追加到地基工具集。
    #[cfg(feature = "codemode-js")]
    pub fn enable_codemode(
        &self,
        workspace: &Path,
        session: Option<&str>,
        mounts: &[String],
    ) -> Result<(), String> {
        let parsed = mounts
            .iter()
            .map(|spec| tower_agent_codemode::Mount::parse(spec))
            .collect::<Result<Vec<_>, _>>()?;
        let root = match session {
            Some(name) => tower_agent_codemode::SessionRoot::Persistent(
                tower_agent_codemode::default_session_dir(name)
                    .ok_or_else(|| "拿不到配置目录".to_owned())?,
            ),
            None => tower_agent_codemode::SessionRoot::Ephemeral,
        };
        #[cfg(feature = "codemode-python")]
        let exec_surface = self
            .mods
            .as_ref()
            .map(|_| {
                tower_agent_codemode::ExecCapabilitySurface::new(root.clone(), parsed.clone())
                    .map(Arc::new)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        let plugin = tower_agent_codemode::CodemodePlugin::new(workspace, root)
            .with_mounts(parsed)
            .with_permission_policy(self.permission_policy.clone())
            .with_hook_registry(Arc::clone(&self.script_hooks));
        #[cfg(feature = "codemode-python")]
        if let Some(mods) = &self.mods {
            mods.register_static_plugin(Arc::new(plugin))
                .map_err(|error| error.to_string())?;
            if let Some(surface) = exec_surface {
                mods.set_capability_surface(surface);
            }
            self.refresh_tools();
            return Ok(());
        }
        let mut context = tower_agent_plugin::PluginContext::new();
        plugin
            .install(&mut context)
            .map_err(|error| error.to_string())?;
        self.base_tools
            .lock()
            .expect("base tools poisoned")
            .extend(context.tools().iter().cloned());
        self.refresh_tools();
        Ok(())
    }

    /// 用显式模型构造会话（空 transcript）。
    pub fn with_model(cwd: impl AsRef<Path>, stream: BoxStreamService, model: Model) -> Self {
        Self::with_history(cwd, stream, model, Vec::new())
    }

    /// 用显式模型 + 初始 transcript 构造会话（续接历史记录时用）。
    pub fn with_history(
        cwd: impl AsRef<Path>,
        stream: BoxStreamService,
        model: Model,
        messages: Vec<AgentMessage>,
    ) -> Self {
        Self::build(cwd, stream, model, messages, None, true)
    }

    /// 用显式模型 + 初始 transcript + 上下文压缩器构造会话。
    ///
    /// 压缩器交给 agent 循环：每次模型请求前按阈值自动压缩（核心能力，与终端无关）。
    pub fn with_compaction(
        cwd: impl AsRef<Path>,
        stream: BoxStreamService,
        model: Model,
        messages: Vec<AgentMessage>,
        compactor: Arc<Compactor>,
    ) -> Self {
        Self::build(cwd, stream, model, messages, Some(compactor), true)
    }

    /// 构造会话，并显式指定是否启用 codemode 宿主。
    pub fn with_compaction_and_codemode(
        cwd: impl AsRef<Path>,
        stream: BoxStreamService,
        model: Model,
        messages: Vec<AgentMessage>,
        compactor: Arc<Compactor>,
        codemode_enabled: bool,
    ) -> Self {
        Self::build(
            cwd,
            stream,
            model,
            messages,
            Some(compactor),
            codemode_enabled,
        )
    }

    fn build(
        cwd: impl AsRef<Path>,
        stream: BoxStreamService,
        model: Model,
        messages: Vec<AgentMessage>,
        compactor: Option<Arc<Compactor>>,
        codemode_enabled: bool,
    ) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        let system_prompt = format!(
            "你是 tower-agent（pi 的 Rust 移植版）的编码助手。可用工具：{}。\n\
         使用工具而不是猜测文件内容；修改文件前先读一遍。",
            tower_agent::tools::DEFAULT_TOOL_NAMES.join(", ")
        );
        let base_tools = tower_agent::assembly::HostPlugins::new(&cwd)
            .tools()
            .expect("内置工具插件应可解析");
        let script_hooks = Arc::new(ScriptHookRegistry::new());
        #[cfg(not(feature = "codemode-python"))]
        let _ = codemode_enabled;
        // mods（cc `mods` 模式）：LLM 生成 monty 代码当插件，可提供工具、改 UI。
        // 内置静态插件与 JIT mod 共用一张注册表：工具目录来自「解析 + 安装」。
        // monty 解释器随 `tower-agent-monty` 内嵌（CLI feature `codemode-python`）。
        #[cfg(feature = "codemode-python")]
        let mods = if codemode_enabled {
            // 插件集合与 mods 宿主都来自组装层（含 wasmtime 等受保护宿主能力），
            // 不在会话里硬编码。
            match tower_agent::assembly::HostPlugins::new(&cwd).mods(UiState::new()) {
                Ok(manager) => {
                    // 把工作目录只读挂到 /work，供 mod 读取项目文件；写操作被拒。
                    let _ = manager.mount_readonly("/work", &cwd);
                    manager.set_hook_registry(Arc::clone(&script_hooks));
                    Some(manager)
                }
                Err(error) => {
                    eprintln!("codemode Python 宿主初始化失败（本次会话已关闭）：{error}");
                    None
                }
            }
        } else {
            None
        };

        #[cfg(feature = "codemode-python")]
        let initial_tools = match &mods {
            Some(mods) => mods.tools(),
            None => base_tools.clone(),
        };
        #[cfg(not(feature = "codemode-python"))]
        let initial_tools = base_tools.clone();

        let mut options = AgentOptions::new(stream);
        options.initial.tools = initial_tools;
        options.initial.model = Some(model);
        options.initial.system_prompt = Some(system_prompt);
        options.initial.messages = messages;
        options.compactor = compactor.clone();
        options.before_tool_call = Some(script_before_tool_call(Arc::clone(&script_hooks)));
        options.after_tool_call = Some(script_after_tool_result(Arc::clone(&script_hooks)));
        let (service, control) = crate::stack::assemble(options);
        let providers = Arc::new(
            tower_agent::assembly::HostPlugins::new(&cwd)
                .providers()
                .expect("built-in provider plugins must assemble"),
        );
        Self {
            service: Mutex::new(service),
            control,
            cwd,
            base_tools: Mutex::new(base_tools),
            #[cfg(feature = "codemode-python")]
            mods,
            tool_filter: Mutex::new(ToolFilter::default()),
            codemode_engine: Mutex::new(ScriptLanguage::JavaScript),
            permission_policy: PermissionPolicy::unattended(),
            script_hooks,
            store: None,
            transcript: None,
            compactor,
            providers,
            provider_credentials: Mutex::new(BTreeMap::new()),
        }
    }

    /// 接入 transcript 持久化句柄。
    #[must_use]
    pub fn with_transcript(mut self, transcript: Arc<crate::store::Transcript>) -> Self {
        self.transcript = Some(transcript);
        self
    }

    /// 接入配置存储：`/login` `/model` `/thinking` 会把结果写回
    /// （对齐 pi 的 `auth.json` / `settings.json`）。
    #[must_use]
    pub fn with_store(mut self, store: Arc<CliStore>) -> Self {
        #[cfg(feature = "codemode-python")]
        if let Some(mods) = &self.mods {
            mods.set_script_store(
                store.script_store(),
                ConfigScope::from_working_dir(&self.cwd),
            );
        }
        self.store = Some(store);
        self
    }

    /// Replace the provider plugin registry used by session commands.
    #[must_use]
    pub fn with_provider_registry(mut self, providers: Arc<ProviderRegistry>) -> Self {
        self.providers = providers;
        self
    }

    /// Seed provider credentials from persisted storage for runtime provider switches.
    #[must_use]
    pub fn with_provider_credentials(
        mut self,
        credentials: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.provider_credentials
            .get_mut()
            .expect("provider credential mutex poisoned")
            .extend(credentials);
        self
    }

    /// Provider plugin ids available to this session.
    #[must_use]
    pub fn provider_ids(&self) -> String {
        self.providers
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id)
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Attach the shared runtime permission policy used by both codemode engines.
    #[must_use]
    pub fn with_permission_policy(mut self, permission_policy: PermissionPolicy) -> Self {
        #[cfg(feature = "codemode-python")]
        if let Some(mods) = &self.mods {
            mods.set_permission_policy(permission_policy.clone());
        }
        self.permission_policy = permission_policy;
        self
    }

    /// Shared script hook registry used by monty and JavaScript codemode hosts.
    #[must_use]
    pub fn script_hooks(&self) -> Arc<ScriptHookRegistry> {
        Arc::clone(&self.script_hooks)
    }

    /// 会话的工作目录。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 订阅会话事件流（交给 TUI 或自定义渲染器）。
    pub fn subscribe(&self) -> Subscription {
        self.control.subscribe()
    }

    /// 当前状态快照。
    pub fn state(&self) -> AgentState {
        self.control.state()
    }

    /// 发一条用户消息并等待整轮结束（助手回复 + 工具执行都完成）。
    pub async fn ask(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.turn(vec![user_message(text)]).await
    }

    /// 从当前 transcript 继续一轮（最后一条必须不是 assistant）。
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        self.turn(Vec::new()).await
    }

    /// 提交一条用户输入。
    ///
    /// 空闲时直接开启新一轮；正在跑的时候用 `steer` 注入，等当前 turn 结束后生效——
    /// 这样用户在 agent 工作期间继续输入不会撞上 "already processing"。
    pub async fn submit(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let text = text.into();
        if self.control.is_streaming() {
            self.control.steer(user_message(text));
            Ok(())
        } else {
            self.turn(vec![user_message(text)]).await
        }
    }

    /// 取消进行中的一轮。
    pub fn abort(&self) {
        self.control.abort();
    }

    /// 替换当前模型（`--model` 与 TUI 的 `/model` 都走这里）。
    pub fn set_model(&self, model: Model) {
        self.control.set_model(model);
    }

    /// 替换思考级别（`--thinking` 与 `/thinking`）。
    pub fn set_thinking_level(&self, level: ModelThinkingLevel) {
        self.control.set_thinking_level(level);
    }

    /// 设置 codemode 的进程级默认引擎。
    pub fn set_codemode_engine(&self, language: ScriptLanguage) {
        *self
            .codemode_engine
            .lock()
            .expect("codemode engine poisoned") = language;
    }

    /// 当前 codemode 的进程级默认引擎。
    #[must_use]
    pub fn codemode_engine(&self) -> ScriptLanguage {
        *self
            .codemode_engine
            .lock()
            .expect("codemode engine poisoned")
    }

    /// 解析某个脚本实际使用的引擎；脚本声明覆盖进程级默认值。
    #[must_use]
    pub fn resolve_codemode_engine(&self, metadata: Option<ScriptMetadata>) -> ScriptLanguage {
        metadata
            .and_then(|metadata| metadata.language)
            .unwrap_or_else(|| self.codemode_engine())
    }

    /// 写回一项配置（未接入存储时是空操作）。
    fn persist_config(&self, key: &str, value: serde_json::Value) {
        if let Some(store) = &self.store {
            // 运行期命令（`/model` `/thinking`）写的是**用户级**偏好；项目级由 CLI
            // 显式指定作用域（`--config-scope project`）写入。
            store.queue_set_config(ConfigScope::User, key, value);
        }
    }

    /// 设置工具过滤（`--tools` / `--exclude-tools` / `--no-tools`）。
    ///
    /// 过滤作用在**目录**（mods 注册表解析结果，或未启用 mods 时的内置工具）之上；
    /// mods 作者工具不受过滤影响（否则就没法写 mod 了）。
    pub fn set_tool_filter(&self, filter: ToolFilter) {
        *self.tool_filter.lock().expect("tool filter poisoned") = filter;
        self.refresh_tools();
    }

    /// mods 的共享 UI 状态（未启用 mods 时为 `None`）。
    #[must_use]
    pub fn ui_state(&self) -> Option<UiState> {
        #[cfg(feature = "codemode-python")]
        {
            self.mods.as_ref().map(|mods| mods.ui())
        }
        #[cfg(not(feature = "codemode-python"))]
        {
            None
        }
    }

    /// 后台预热 mods 沙箱（编译组件 + 预建实例）；在运行时内调用才生效。
    pub fn prewarm_mods(&self) {
        #[cfg(feature = "codemode-python")]
        if let Some(mods) = &self.mods {
            mods.spawn_warmup();
        }
    }

    /// 加载持久化的 mod（`dirs` 的 config 目录）；返回加载数量。
    pub async fn load_persisted_mods(&self) -> usize {
        #[cfg(feature = "codemode-python")]
        {
            match &self.mods {
                Some(mods) => mods.load_persisted().await,
                None => 0,
            }
        }
        #[cfg(not(feature = "codemode-python"))]
        {
            0
        }
    }

    /// 从 `dirs` 的 config 目录（`tower-agent/plugins`）加载动态插件。
    ///
    /// Missing libraries are not fatal. Callers decide how to surface
    /// [`DynamicPluginLoad::shell_notice`].
    #[allow(unsafe_code)] // 加载用户配置目录里的动态库，视作受信任。
    pub fn load_dynamic_plugins(&self) -> DynamicPluginLoad {
        #[cfg(feature = "codemode-python")]
        {
            let Some(mods) = &self.mods else {
                return DynamicPluginLoad {
                    loaded: Vec::new(),
                    error: Some("动态插件宿主未启用（需要 `codemode-python` feature）".to_owned()),
                };
            };
            match unsafe { mods.load_dynamic_plugins_from_config() } {
                Ok(loaded) => DynamicPluginLoad {
                    loaded,
                    error: None,
                },
                Err(error) => DynamicPluginLoad {
                    loaded: Vec::new(),
                    error: Some(format!("动态插件加载失败：{error}")),
                },
            }
        }
        #[cfg(not(feature = "codemode-python"))]
        {
            DynamicPluginLoad {
                loaded: Vec::new(),
                error: Some("动态插件宿主未启用（缺少 `codemode-python` feature）".to_owned()),
            }
        }
    }

    /// 重新计算工具集：目录（静态 + JIT 的统一解析结果）经过滤 + mods 作者工具。
    ///
    /// mod 在运行中 `tool_register` 之后，下一轮 turn 开始前会走到这里，因此新工具
    /// 会被 agent 看到。
    fn refresh_tools(&self) {
        #[cfg(feature = "codemode-python")]
        let catalog = match &self.mods {
            Some(mods) => mods.tools(),
            None => self.base_tools.lock().expect("base tools poisoned").clone(),
        };
        #[cfg(not(feature = "codemode-python"))]
        let catalog = self.base_tools.lock().expect("base tools poisoned").clone();
        let filter = self
            .tool_filter
            .lock()
            .expect("tool filter poisoned")
            .clone();
        // 地基（作者工具）不受 `--tools` 过滤影响，否则就没法写 mod 了。
        let tools: Vec<AgentTool> = catalog
            .into_iter()
            .filter(|tool| filter.allows(tool.name()) || is_host_reserved(tool.name()))
            .collect();
        self.control.set_tools(tools);
    }

    /// 重置会话（`/new`）：清空 transcript 与队列，保留 system prompt 与工具。
    pub fn reset(&self) -> Result<(), AgentError> {
        self.control.reset()
    }

    /// 走 Service 边界跑一轮：`messages` 为空表示继续当前 transcript。
    async fn turn(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        // 上一轮 mod 注册的工具在这一轮生效。
        self.refresh_tools();
        let request = AgentRequest {
            messages,
            ..AgentRequest::default()
        };
        let service = self
            .service
            .lock()
            .expect("session service poisoned")
            .clone();
        let outcome = service.oneshot(request).await.map(|_| ());
        // 本轮 mod 注册的工具在下一轮生效。
        self.refresh_tools();
        // 无论成功 / 取消 / 失败，都把本轮新增的消息落盘（`sync` 只追加尾巴）。
        self.persist_transcript().await;
        outcome
    }

    /// 把当前 transcript 的未落盘部分写进存储（未接入存储时是空操作）。
    async fn persist_transcript(&self) {
        let Some(transcript) = &self.transcript else {
            return;
        };
        let state = self.state();
        let compaction = self
            .compactor
            .as_ref()
            .and_then(|compactor| compactor.current());
        if let Err(error) = transcript
            .sync(&state.messages, Some(&state.model.id), compaction.as_ref())
            .await
        {
            eprintln!("transcript 落盘失败：{error}");
        }
    }
}

/// 地基作者工具名（`codemode_write` 等）：不受 `--tools` 过滤影响，否则就没法写脚本了。
///
/// 没有编译进 mods 能力时不存在作者工具，一律不算保留名。
fn is_host_reserved(name: &str) -> bool {
    #[cfg(feature = "codemode-python")]
    {
        tower_agent_monty::is_reserved_name(name)
    }
    #[cfg(not(feature = "codemode-python"))]
    {
        let _ = name;
        false
    }
}

fn script_before_tool_call(
    hooks: Arc<ScriptHookRegistry>,
) -> tower_agent_runtime::agent_loop::BeforeToolCall {
    Arc::new(move |context| {
        let hooks = Arc::clone(&hooks);
        Box::pin(async move {
            if hooks.is_empty() {
                return None;
            }
            let mut arguments = serde_json::Value::Object(context.args.clone());
            let results = hooks
                .invoke(
                    tower_agent_codemode_core::ScriptHookEvent::ToolCall,
                    serde_json::json!({
                        "name": context.tool_call.name,
                        "arguments": arguments,
                    }),
                    Some(context.signal.clone()),
                )
                .await;
            let mut changed = false;
            for hook in results {
                let value = match hook.value {
                    Ok(value) => value,
                    Err(error) => {
                        return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                            block: true,
                            reason: Some(error.to_string()),
                            ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                        });
                    }
                };
                match value {
                    serde_json::Value::Null | serde_json::Value::Bool(true) => {}
                    serde_json::Value::Bool(false) => {
                        return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                            block: true,
                            reason: Some("blocked by script tool_call hook".to_owned()),
                            ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                        });
                    }
                    serde_json::Value::String(reason) => {
                        return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                            block: true,
                            reason: Some(reason),
                            ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                        });
                    }
                    serde_json::Value::Object(object) => {
                        if object.get("action").and_then(serde_json::Value::as_str) == Some("block")
                        {
                            return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                                block: true,
                                reason: Some(
                                    object
                                        .get("message")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or("blocked by script tool_call hook")
                                        .to_owned(),
                                ),
                                ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                            });
                        }
                        if let Some(next) = object.get("arguments") {
                            if !next.is_object() {
                                return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                                    block: true,
                                    reason: Some(
                                        "tool_call hook arguments must be an object".to_owned(),
                                    ),
                                    ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                                });
                            }
                            arguments = next.clone();
                            changed = true;
                        }
                    }
                    _ => {
                        return Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                            block: true,
                            reason: Some(
                                "tool_call hook must return null, bool, string, or object"
                                    .to_owned(),
                            ),
                            ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
                        });
                    }
                }
            }
            Some(tower_agent_runtime::agent_loop::BeforeToolCallResult {
                arguments: changed.then(|| arguments.as_object().cloned().unwrap_or_default()),
                ..tower_agent_runtime::agent_loop::BeforeToolCallResult::default()
            })
        })
    })
}

fn script_after_tool_result(
    hooks: Arc<ScriptHookRegistry>,
) -> tower_agent_runtime::agent_loop::AfterToolCall {
    Arc::new(move |context| {
        let hooks = Arc::clone(&hooks);
        Box::pin(async move {
            if hooks.is_empty() {
                return None;
            }
            let mut text = context
                .result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut is_error = context.is_error;
            let results = hooks
                .invoke(
                    tower_agent_codemode_core::ScriptHookEvent::ToolResult,
                    serde_json::json!({
                        "name": context.tool_call.name,
                        "arguments": context.args,
                        "text": text,
                        "is_error": is_error,
                    }),
                    Some(context.signal.clone()),
                )
                .await;
            let mut changed = false;
            for hook in results {
                match hook.value {
                    Ok(serde_json::Value::Null) => {}
                    Ok(serde_json::Value::String(next)) => {
                        text = next;
                        changed = true;
                    }
                    Ok(serde_json::Value::Object(object)) => {
                        if object.get("action").and_then(serde_json::Value::as_str) == Some("block")
                        {
                            text = object
                                .get("message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("tool result blocked by script hook")
                                .to_owned();
                            is_error = true;
                            changed = true;
                            continue;
                        }
                        if let Some(next) = object.get("text").and_then(serde_json::Value::as_str) {
                            text = next.to_owned();
                            changed = true;
                        }
                        if let Some(next) =
                            object.get("is_error").and_then(serde_json::Value::as_bool)
                        {
                            is_error = next;
                            changed = true;
                        }
                    }
                    Ok(other) => {
                        text = format!("tool_result hook returned unsupported value: {other}");
                        is_error = true;
                        changed = true;
                    }
                    Err(error) => {
                        text = error.to_string();
                        is_error = true;
                        changed = true;
                    }
                }
            }
            changed.then(|| tower_agent_runtime::agent_loop::AfterToolCallResult {
                content: Some(vec![ContentBlock::Text(TextContent {
                    text,
                    text_signature: None,
                })]),
                is_error: Some(is_error),
                ..tower_agent_runtime::agent_loop::AfterToolCallResult::default()
            })
        })
    })
}

/// `off` / `minimal` / … / `max` → [`ModelThinkingLevel`]（pi `--thinking` / `/thinking` 的取值）。
#[must_use]
pub fn parse_thinking_level(value: &str) -> Option<ModelThinkingLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::Minimal,
        )),
        "low" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::Low,
        )),
        "medium" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::Medium,
        )),
        "high" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::High,
        )),
        "xhigh" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::XHigh,
        )),
        "max" => Some(ModelThinkingLevel::Level(
            tower_agent_provider::ThinkingLevel::Max,
        )),
        _ => None,
    }
}

/// [`ModelThinkingLevel`] → pi 的取值字符串。
#[must_use]
pub fn thinking_level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::Minimal) => "minimal",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::Low) => "low",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::Medium) => "medium",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::High) => "high",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::XHigh) => "xhigh",
        ModelThinkingLevel::Level(tower_agent_provider::ThinkingLevel::Max) => "max",
    }
}

/// pi 的思考级别取值（帮助文案与错误提示共用）。
pub const THINKING_LEVELS: &str = "off, minimal, low, medium, high, xhigh, max";

impl tower_agent_ui::CommandHost for Session {
    fn new_session(&self) -> Result<(), String> {
        self.reset().map_err(|error| error.to_string())?;
        // `/new` 开一个新会话记录：换 id、清零「已落盘条数」，下一轮 sync 写新的会话。
        if let Some(transcript) = &self.transcript {
            transcript.reset();
        }
        Ok(())
    }

    fn session_info(&self) -> Vec<String> {
        let state = self.state();
        let mut lines = vec![
            format!("cwd: {}", self.cwd().display()),
            format!(
                "model: {} (provider {} / api {})",
                state.model.id, state.model.provider, state.model.api
            ),
            format!("thinking: {}", thinking_level_name(state.thinking_level)),
            format!(
                "messages: {} · tools: {}",
                state.messages.len(),
                state.tools.len()
            ),
            format!(
                "state: {}",
                if state.is_streaming {
                    "running"
                } else {
                    "idle"
                }
            ),
        ];
        if let Some(transcript) = &self.transcript {
            lines.insert(0, format!("session: {}", transcript.id()));
        }
        if let Some(compactor) = &self.compactor {
            let settings = compactor.settings();
            lines.push(format!(
                "compaction: {} (reserve {} / keep {})",
                if settings.enabled { "on" } else { "off" },
                settings.reserve_tokens,
                settings.keep_recent_tokens
            ));
        }
        lines
    }

    fn model(&self, arg: Option<&str>) -> Result<String, String> {
        let current = self.state().model;
        let Some(arg) = arg else {
            return Ok(format!(
                "current model: {} (provider {} / api {})\n\
                 available providers: {}\n\
                 usage: /model <provider>/<id>, /model <provider>, or /model <id>",
                current.id,
                current.provider,
                current.api,
                self.provider_ids()
            ));
        };

        let (requested_provider, requested_model) = match arg.split_once('/') {
            Some((provider, model)) => (Some(provider.trim()), model.trim()),
            None => (None, arg.trim()),
        };

        let provider = requested_provider
            .filter(|provider| !provider.is_empty())
            .or_else(|| {
                self.providers
                    .descriptor(requested_model)
                    .map(|_| requested_model)
            })
            .unwrap_or(&current.provider);
        let descriptor = self.providers.descriptor(provider).ok_or_else(|| {
            format!(
                "unknown provider: {provider} (available: {})",
                self.provider_ids()
            )
        })?;
        let switched_by_provider_name =
            requested_provider.is_none() && descriptor.id == requested_model;
        let model_id = if switched_by_provider_name {
            descriptor
                .default_model
                .as_deref()
                .ok_or_else(|| format!("provider `{provider}` does not declare a default model"))?
        } else {
            if requested_model.is_empty() {
                return Err("model id must not be empty. usage: /model <provider>/<id>".into());
            }
            requested_model
        };

        let stored = self
            .provider_credentials
            .lock()
            .expect("provider credential mutex poisoned")
            .get(provider)
            .cloned();
        let api_key = crate::provider::resolve_api_key_for_provider(
            &self.providers,
            provider,
            None,
            stored.as_deref(),
        );
        let setup = crate::provider::build_from_registry(
            &self.providers,
            provider,
            Some(model_id),
            crate::provider::ProviderStackOptions::default(),
            api_key.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        self.control.set_stream(setup.stream_fn());
        self.set_model(setup.model.clone());
        // 写回 pi `settings.json` 的 `defaultProvider` / `defaultModel`。
        self.persist_config(
            crate::store::DEFAULT_PROVIDER,
            serde_json::json!(setup.model.provider),
        );
        self.persist_config(
            crate::store::DEFAULT_MODEL,
            serde_json::json!(setup.model.id),
        );
        Ok(format!(
            "model switched to: {} (provider {})",
            setup.model.id, setup.model.provider
        ))
    }

    fn thinking(&self, arg: Option<&str>) -> Result<String, String> {
        match arg {
            None => Ok(format!(
                "current thinking level: {}\nusage: /thinking <{}>",
                thinking_level_name(self.state().thinking_level),
                THINKING_LEVELS
            )),
            Some(value) => match parse_thinking_level(value) {
                Some(level) => {
                    self.set_thinking_level(level);
                    // 写回 pi `settings.json` 的 `defaultThinkingLevel`。
                    self.persist_config(
                        crate::store::DEFAULT_THINKING_LEVEL,
                        serde_json::json!(thinking_level_name(level)),
                    );
                    Ok(format!(
                        "thinking level set to: {}",
                        thinking_level_name(level)
                    ))
                }
                None => Err(format!(
                    "unknown thinking level: {value} (available: {THINKING_LEVELS})"
                )),
            },
        }
    }

    fn login(&self, arg: Option<&str>) -> Result<String, String> {
        let Some(arg) = arg else {
            return Ok(format!(
                "usage: /login <provider> <api-key>\navailable providers: {}",
                self.provider_ids()
            ));
        };
        let mut parts = arg.splitn(2, char::is_whitespace);
        let provider = parts.next().unwrap_or_default().trim();
        let api_key = parts.next().map(str::trim).filter(|key| !key.is_empty());
        let descriptor = self.providers.descriptor(provider).ok_or_else(|| {
            format!(
                "unknown provider: {provider} (available: {})",
                self.provider_ids()
            )
        })?;

        // 没带 key 时退回环境变量；两条路都缺凭据就报错。
        let options = crate::provider::ProviderStackOptions::default();
        let api_key = crate::provider::resolve_api_key_for_provider(
            &self.providers,
            &descriptor.id,
            api_key,
            None,
        );
        let setup = crate::provider::build_from_registry(
            &self.providers,
            &descriptor.id,
            descriptor.default_model.as_deref(),
            options,
            api_key.as_deref(),
        )
        .map_err(|error| error.to_string())?;

        self.control.set_stream(setup.stream_fn());
        self.control.set_model(setup.model.clone());
        if let Some(api_key) = api_key.as_ref() {
            self.provider_credentials
                .lock()
                .expect("provider credential mutex poisoned")
                .insert(descriptor.id.clone(), api_key.clone());
        }
        // 对齐 pi：`/login` 把凭据写进 `auth.json`、默认 provider / 模型写进 `settings.json`。
        if let Some(store) = &self.store {
            let model_id = setup.model.id.clone();
            // 只持久化显式给出的 key；`/login <provider>`（回退到环境变量）不把环境里的 key 落盘。
            if let Some(api_key) = api_key {
                store.queue_put_credential(ProviderCredential {
                    provider: descriptor.id.clone(),
                    api_key,
                    model: Some(model_id.clone()),
                    base_url: None,
                    updated_at: tower_agent_runtime::agent_loop::now_ms(),
                });
            }
            store.queue_set_config(
                ConfigScope::User,
                crate::store::DEFAULT_PROVIDER,
                serde_json::json!(descriptor.id),
            );
            store.queue_set_config(
                ConfigScope::User,
                crate::store::DEFAULT_MODEL,
                serde_json::json!(model_id),
            );
        }
        Ok(format!(
            "provider configured: {} (model {})",
            descriptor.id, setup.model.id
        ))
    }

    fn compact(&self, arg: Option<&str>) -> Result<String, String> {
        let Some(compactor) = &self.compactor else {
            return Err("compaction is not enabled for this session".into());
        };
        compactor.request_compaction(arg.map(str::to_owned));
        Ok(match arg {
            Some(instructions) => {
                format!("Compaction requested ({instructions}); it runs before the next request.")
            }
            None => "Compaction requested; it runs before the next model request.".into(),
        })
    }
}

/// 构造一条用户消息。
fn user_message(text: impl Into<String>) -> AgentMessage {
    AgentMessage::Message(Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: tower_agent_runtime::agent_loop::now_ms(),
    }))
}

/// provider 未上报模型时的占位模型。
fn default_model() -> Model {
    Model {
        id: "unknown".into(),
        name: "unknown".into(),
        api: "unknown".into(),
        provider: "unknown".into(),
        ..Model::default()
    }
}

/// 把事件流打印成纯文本（`--print`）。
///
/// 只打印对人有意义的事件：用户/助手消息、工具开始与结束、错误。
pub async fn run_print<W: Write>(
    mut subscription: Subscription,
    mut out: W,
) -> std::io::Result<()> {
    while let Some(event) = subscription.recv().await {
        let is_end = matches!(event, AgentEvent::AgentEnd { .. });
        write_event(&mut out, &event)?;
        if is_end {
            break;
        }
    }
    out.flush()
}

/// 单条事件的文本渲染。
pub fn write_event<W: Write>(out: &mut W, event: &AgentEvent) -> std::io::Result<()> {
    match event {
        AgentEvent::AgentStart => writeln!(out, "[agent] start"),
        AgentEvent::AgentEnd { .. } => writeln!(out, "[agent] end"),
        AgentEvent::MessageEnd { message } => match message {
            tower_agent_runtime::AgentMessage::Message(Message::User(_)) => {
                writeln!(out, "[user] {}", tower_agent_ui::render_message(message))
            }
            tower_agent_runtime::AgentMessage::Message(Message::Assistant(_)) => {
                writeln!(
                    out,
                    "[assistant] {}",
                    tower_agent_ui::render_message(message)
                )
            }
            tower_agent_runtime::AgentMessage::Message(Message::System(_)) => Ok(()),
            // 工具结果由 tool_execution_end 那行呈现。
            _ => Ok(()),
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => writeln!(out, "[tool] {tool_name} {args}"),
        AgentEvent::ToolExecutionEnd {
            tool_name,
            result,
            is_error,
            ..
        } => writeln!(
            out,
            "[tool:{}] {tool_name} {}",
            if *is_error { "error" } else { "ok" },
            tower_agent_ui::render_message(&tower_agent_runtime::AgentMessage::Message(
                Message::ToolResult(tower_agent_provider::ToolResultMessage {
                    tool_call_id: String::new(),
                    tool_name: tool_name.clone(),
                    content: result.content.clone(),
                    details: None,
                    is_error: *is_error,
                    timestamp: 0,
                })
            ))
        ),
        _ => Ok(()),
    }
}

/// 用 TUI 渲染这条会话：先订阅，再跑一轮，最后交给 [`tower_agent_tui::run_agent_tui`]。
///
/// 订阅事件、把输入区提交的文本交给 agent，直到用户退出；`q`/`Esc`/`Ctrl+C` 退出。
///
/// 编辑器默认 vim 普通模式（`i` 插入 / `Esc` 普通 / `Enter` 提交 / `q` 退出）。
/// 用户退出时会取消还在进行的一轮，等驱动任务收尾后返回。
pub async fn run_tui(
    session: &Arc<Session>,
    prompt: Option<String>,
    title: &str,
    notice: Option<String>,
) -> std::io::Result<()> {
    // 先恢复持久化的 mod、加载动态插件，再后台预热沙箱。
    session.load_persisted_mods().await;
    let plugin_load = session.load_dynamic_plugins();
    let notice = combine_notice(notice, plugin_load.shell_notice());
    session.prewarm_mods();
    let subscription = session.subscribe();
    let (submissions_tx, mut submissions_rx) = mpsc::unbounded_channel::<String>();
    // 斜杠命令（`/new`、`/model`…）在 TUI 里执行，通过这个 trait 回调到会话。
    let host: Arc<dyn tower_agent_ui::CommandHost> = session.clone();

    let driver_session = Arc::clone(session);
    let driver = tokio::spawn(async move {
        let mut first_error: Option<AgentError> = None;
        if let Some(prompt) = prompt
            && let Err(error) = driver_session.submit(prompt).await
        {
            first_error = Some(error);
        }
        while let Some(text) = submissions_rx.recv().await {
            if let Err(error) = driver_session.submit(text).await {
                first_error.get_or_insert(error);
            }
        }
        first_error
    });

    let plugins = tower_agent_tui::frontend_plugins();
    #[cfg(feature = "codemode-python")]
    let plugins = {
        let mut plugins = plugins;
        if let Some(mods) = &session.mods {
            plugins.push(Box::new(tower_agent_tui::ContributionPlugin::new(
                mods.ui(),
            )));
        }
        plugins
    };

    let rendered = tower_agent_tui::run_agent_tui(
        subscription,
        submissions_tx,
        host,
        title.to_string(),
        notice,
        plugins,
    )
    .await;

    // 用户退出：取消进行中的一轮，并等驱动任务自然收尾（避免直接 abort 丢掉取消语义）。
    session.abort();
    let driver_result = driver.await;

    rendered?;
    if let Ok(Some(error)) = driver_result {
        return Err(std::io::Error::other(error.to_string()));
    }
    Ok(())
}

fn combine_notice(existing: Option<String>, warning: Option<String>) -> Option<String> {
    match (existing, warning) {
        (Some(existing), Some(warning)) => Some(format!("{existing}\n{warning}")),
        (None, Some(warning)) => Some(warning),
        (existing, None) => existing,
    }
}

#[cfg(test)]
mod tests {
    //! 会话入口的端到端测试：用离线 faux provider 驱动真实的 agent 循环 + 真实内置工具。

    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use futures_util::{future::BoxFuture, stream};
    use tokio::sync::Semaphore;
    use tower::Service;
    use tower_agent_provider::{
        AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService,
        ContentBlock, DoneReason, PiError, StopReason, StreamRequest, TextContent, Usage,
    };
    use tower_agent_provider_builtin::provider::faux::{FauxProvider, model};
    use tower_agent_ui::CommandHost as _;

    use super::{
        ConfigScope, DynamicPluginLoad, ScriptLanguage, Session, parse_thinking_level, run_print,
        thinking_level_name,
    };

    #[test]
    fn missing_shell_plugin_produces_a_non_fatal_notice() {
        let load = DynamicPluginLoad::default();
        let notice = load.shell_notice().expect("missing shell warning");
        assert!(notice.contains("shell 工具暂不可用"));
        assert!(notice.contains("其他工具仍可正常使用"));
        assert!(notice.contains("tower-agent/plugins"));
    }

    #[test]
    fn loaded_shell_plugin_suppresses_the_warning() {
        let load = DynamicPluginLoad {
            loaded: vec!["shell".to_owned()],
            error: None,
        };
        assert!(load.shell_notice().is_none());
    }

    fn session(cwd: &std::path::Path) -> Session {
        Session::with_model(cwd, FauxProvider::new().boxed(), model())
    }

    async fn call_codemode_tool(
        tool: &tower_agent_core::AgentTool,
        name: &str,
        arguments: serde_json::Value,
    ) -> String {
        use futures_util::StreamExt as _;
        use tower::ServiceExt as _;

        let mut router = tower_agent_runtime::ToolRouter::new();
        router.insert_boxed(tool.name(), tool.service().clone());
        let request = tower_agent_core::ToolCallRequest::new(tower_agent_provider::ToolCall {
            id: format!("codemode:{name}"),
            name: name.to_owned(),
            arguments: arguments.as_object().cloned().unwrap_or_default().into(),
            thought_signature: None,
            namespace: None,
        });
        let stream = router.oneshot(request).await.expect("route codemode tool");
        let results: Vec<_> = stream.collect().await;
        results[0]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_tool_result_text(
        state: &tower_agent_runtime::AgentState,
        tool_name: &str,
    ) -> (String, bool) {
        let result = state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                tower_agent_runtime::AgentMessage::Message(
                    tower_agent_provider::Message::ToolResult(result),
                ) if result.tool_name == tool_name => Some(result),
                _ => None,
            })
            .unwrap_or_else(|| panic!("last `{tool_name}` tool result"));
        (
            tower_agent_ui::render_message(&tower_agent_runtime::AgentMessage::Message(
                tower_agent_provider::Message::ToolResult(result.clone()),
            )),
            result.is_error,
        )
    }

    #[test]
    fn codemode_engine_defaults_to_js_and_script_overrides_it() {
        use super::{ScriptLanguage, ScriptMetadata};

        let dir = tempfile::tempdir().expect("tempdir");
        let session = session(dir.path());
        assert_eq!(session.codemode_engine(), ScriptLanguage::JavaScript);

        session.set_codemode_engine(ScriptLanguage::Monty);
        assert_eq!(session.resolve_codemode_engine(None), ScriptLanguage::Monty);
        assert_eq!(
            session.resolve_codemode_engine(Some(ScriptMetadata::with_language(
                ScriptLanguage::JavaScript
            ))),
            ScriptLanguage::JavaScript
        );
    }

    #[cfg(feature = "codemode-python")]
    #[test]
    fn no_codemode_disables_the_python_host() {
        use tower_agent_runtime::{CompactionSettings, Compactor};

        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session::with_compaction_and_codemode(
            dir.path(),
            FauxProvider::new().boxed(),
            model(),
            Vec::new(),
            Arc::new(Compactor::new(CompactionSettings::default())),
            false,
        );
        assert!(session.ui_state().is_none());
    }

    /// 第一次调用会卡在闸门上的 provider；之后的调用立刻返回，用来观察 steer 行为。
    #[derive(Clone)]
    struct GatedProvider {
        gate: Arc<Semaphore>,
        calls: Arc<AtomicUsize>,
    }

    impl GatedProvider {
        fn new(gate: Arc<Semaphore>) -> Self {
            Self {
                gate,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn boxed(self) -> BoxStreamService {
            tower::util::BoxCloneService::new(self)
        }
    }

    impl Service<StreamRequest> for GatedProvider {
        type Response = AssistantEventStream;
        type Error = PiError;
        type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: StreamRequest) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let model = request.model.clone();
            Box::pin(async move {
                if call == 0 {
                    let _permit = gate.acquire().await;
                }
                Ok(AssistantEventStream::new(stream::iter(text_events(
                    &model,
                    &format!("reply-{call}"),
                ))))
            })
        }
    }

    /// 一条最简的文本事件流（`start → text_* → done`）。
    fn text_events(model: &tower_agent_provider::Model, text: &str) -> Vec<AssistantMessageEvent> {
        let message = |stop_reason: StopReason, content: Vec<ContentBlock>| AssistantMessage {
            content,
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        };
        let block = |value: &str| {
            ContentBlock::Text(TextContent {
                text: value.into(),
                text_signature: None,
            })
        };
        vec![
            AssistantMessageEvent::Start {
                partial: message(StopReason::Pending, Vec::new()),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: message(StopReason::Pending, vec![block("")]),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
                partial: message(StopReason::Pending, vec![block(text)]),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: text.into(),
                partial: message(StopReason::Pending, vec![block(text)]),
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: message(StopReason::Stop, vec![block(text)]),
            },
        ]
    }

    fn last_assistant_text(state: &tower_agent_runtime::AgentState) -> String {
        state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                tower_agent_runtime::AgentMessage::Message(
                    tower_agent_provider::Message::Assistant(assistant),
                ) => Some(tower_agent_ui::render_message(
                    &tower_agent_runtime::AgentMessage::Message(
                        tower_agent_provider::Message::Assistant(assistant.clone()),
                    ),
                )),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn ask_returns_a_text_reply_without_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("你好").await.expect("turn succeeds");

        let state = session.state();
        assert_eq!(state.messages.len(), 3, "system + user + assistant");
        assert!(last_assistant_text(&state).contains("[faux] 收到：你好"));
        assert!(!state.is_streaming);
    }

    #[tokio::test]
    async fn ask_executes_a_real_tool_and_feeds_the_result_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "工具读到的内容\n").unwrap();
        let session = session(dir.path());

        // faux provider 把 `!read {…}` 翻译成一次工具调用；agent 执行真实 read 工具，
        // 把工具结果写回 transcript，再由 faux provider 收尾。
        session
            .ask("!read {\"path\":\"note.txt\"}")
            .await
            .expect("turn succeeds");

        let state = session.state();
        let roles: Vec<Option<&str>> = state
            .messages
            .iter()
            .map(tower_agent_runtime::AgentMessage::role)
            .collect();
        assert_eq!(
            roles,
            vec![
                Some("system"),
                Some("user"),
                Some("assistant"),
                Some("toolResult"),
                Some("assistant"),
            ],
            "user → tool call → tool result → final reply"
        );

        let tool_result = state
            .messages
            .iter()
            .find_map(|message| match message {
                tower_agent_runtime::AgentMessage::Message(
                    tower_agent_provider::Message::ToolResult(result),
                ) => Some(result),
                _ => None,
            })
            .expect("a tool result message");
        assert_eq!(tool_result.tool_name, "read");
        assert!(!tool_result.is_error);
        assert_eq!(
            tower_agent_ui::render_message(&tower_agent_runtime::AgentMessage::Message(
                tower_agent_provider::Message::ToolResult(tool_result.clone())
            )),
            "工具读到的内容\n"
        );
        assert!(last_assistant_text(&state).contains("工具结果已收到"));
    }

    #[cfg(feature = "codemode-python")]
    #[tokio::test]
    async fn python_script_hooks_change_agent_behavior() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("original.txt"), "ORIGINAL\n").unwrap();
        std::fs::write(dir.path().join("redirected.txt"), "SECRET\n").unwrap();
        let session = session(dir.path());
        let manager = session.mods.as_ref().expect("Python codemode host");
        let script = r#"
tool_register("script_echo", "script_echo", "", {"type": "object"})

def script_echo(args):
    return "tool:" + args["text"]

def guard(args):
    if args["name"] != "read":
        return None
    return {"action": "allow", "arguments": {"path": "redirected.txt"}}

def redact(args):
    if args["name"] != "read":
        return None
    return {"text": "REDACTED " + args["arguments"]["path"]}

on("tool_call", "guard")
on("tool_result", "redact")
"#;
        manager
            .write("behavior", script)
            .await
            .expect("write hooks");
        let echo = manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "script_echo")
            .expect("registered script tool");
        assert_eq!(
            call_codemode_tool(&echo, "script_echo", serde_json::json!({ "text": "ok" })).await,
            "tool:ok"
        );

        session
            .ask("!read {\"path\":\"original.txt\"}")
            .await
            .expect("hooked read");
        let (text, is_error) = last_tool_result_text(&session.state(), "read");
        assert!(!is_error, "{text}");
        assert_eq!(text, "REDACTED redirected.txt");

        manager
            .write(
                "behavior",
                r#"
def block(args):
    if args["name"] == "read":
        return {"action": "block", "message": "danger blocked"}
    return None

on("tool_call", "block")
"#,
            )
            .await
            .expect("write blocking hook");
        session
            .ask("!read {\"path\":\"redirected.txt\"}")
            .await
            .expect("blocked read");
        let (text, is_error) = last_tool_result_text(&session.state(), "read");
        assert!(is_error);
        assert_eq!(text, "danger blocked");
    }

    #[cfg(feature = "codemode-js")]
    #[tokio::test]
    async fn js_script_hooks_change_agent_behavior() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("original.txt"), "ORIGINAL\n").unwrap();
        std::fs::write(dir.path().join("redirected.txt"), "SECRET\n").unwrap();
        let session = session(dir.path());
        session
            .enable_codemode(dir.path(), None, &[])
            .expect("enable JavaScript codemode");
        let codemode = session
            .state()
            .tools
            .into_iter()
            .find(|tool| tool.name() == "codemode")
            .expect("codemode tool");

        let script = r#"
function script_echo(args) { return "tool:" + args.text; }
tool_register("script_echo", "script_echo", "", { type: "object" });
function guard(args) {
  if (args.name !== "read") return null;
  return { action: "allow", arguments: { path: "redirected.txt" } };
}
function redact(args) {
  if (args.name !== "read") return null;
  return { text: "REDACTED " + args.arguments.path };
}
on("tool_call", "guard");
on("tool_result", "redact");
"#;
        call_codemode_tool(&codemode, "codemode", serde_json::json!({ "code": script })).await;
        let echoed = call_codemode_tool(
            &codemode,
            "codemode",
            serde_json::json!({ "code": "return await tool_call(\"script_echo\", { text: \"ok\" });" }),
        )
        .await;
        assert!(echoed.contains("tool:ok"), "{echoed}");

        session
            .ask("!read {\"path\":\"original.txt\"}")
            .await
            .expect("hooked read");
        let (text, is_error) = last_tool_result_text(&session.state(), "read");
        assert!(!is_error, "{text}");
        assert_eq!(text, "REDACTED redirected.txt");

        call_codemode_tool(
            &codemode,
            "codemode",
            serde_json::json!({
                "code": r#"
function block(args) {
  if (args.name === "read") return { action: "block", message: "danger blocked" };
  return null;
}
on("tool_call", "block");
"#
            }),
        )
        .await;
        session
            .ask("!read {\"path\":\"redirected.txt\"}")
            .await
            .expect("blocked read");
        let (text, is_error) = last_tool_result_text(&session.state(), "read");
        assert!(is_error);
        assert_eq!(text, "danger blocked");
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_as_an_error_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("!nope {}").await.expect("turn succeeds");

        let result = session
            .state()
            .messages
            .iter()
            .find_map(|message| match message {
                tower_agent_runtime::AgentMessage::Message(
                    tower_agent_provider::Message::ToolResult(result),
                ) => Some(result.clone()),
                _ => None,
            })
            .expect("a tool result message");
        assert!(result.is_error, "unknown tool produces an error result");
        assert!(
            tower_agent_ui::render_message(&tower_agent_runtime::AgentMessage::Message(
                tower_agent_provider::Message::ToolResult(result)
            ))
            .contains("Tool nope not found")
        );
    }

    /// 用户在 agent 工作期间继续输入时，`submit` 应该走 steer 而不是报 "already processing"。
    #[tokio::test]
    async fn submit_steers_while_a_turn_is_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let gate = Arc::new(Semaphore::new(0));
        let session = Arc::new(Session::with_model(
            dir.path(),
            GatedProvider::new(Arc::clone(&gate)).boxed(),
            model(),
        ));

        // 第一轮会卡在 provider 的闸门上。
        let first = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.submit("first").await })
        };
        while !session.state().is_streaming {
            tokio::task::yield_now().await;
        }

        // 第二轮输入在流式期间提交：应当被 steer 接受。
        session
            .submit("second")
            .await
            .expect("steering while streaming is accepted");

        // 放行第一轮，steer 的消息会在下一个 turn 被注入。
        gate.add_permits(1);
        first
            .await
            .expect("task joins")
            .expect("first turn succeeds");

        let state = session.state();
        let user_texts: Vec<String> = state
            .messages
            .iter()
            .filter_map(|message| match message {
                tower_agent_runtime::AgentMessage::Message(
                    tower_agent_provider::Message::User(user),
                ) => match &user.content {
                    tower_agent_provider::UserContent::Text(text) => Some(text.clone()),
                    tower_agent_provider::UserContent::Blocks(_) => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(user_texts, vec!["first", "second"]);
        let assistants = state
            .messages
            .iter()
            .filter(|message| matches!(message.role(), Some("assistant")))
            .count();
        assert_eq!(assistants, 2, "steer 触发第二个 turn");
        assert!(!state.is_streaming);
    }

    #[tokio::test]
    async fn session_registers_all_builtin_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        let state = session.state();
        let names: Vec<&str> = state.tools.iter().map(|tool| tool.name()).collect();
        // 内置工具在最前；启用 mods 时其后会追加 `mod_*` 作者工具。
        assert!(
            names.starts_with(&tower_agent::tools::DEFAULT_TOOL_NAMES),
            "内置工具应按顺序领先：{names:?}"
        );
        assert!(state.system_prompt.contains("read"));
    }

    #[tokio::test]
    async fn print_renderer_writes_the_whole_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "内容\n").unwrap();
        let session = session(dir.path());
        let subscription = session.subscribe();

        let mut out: Vec<u8> = Vec::new();
        let (asked, printed) = tokio::join!(
            session.ask("!read {\"path\":\"note.txt\"}"),
            run_print(subscription, &mut out)
        );
        asked.expect("turn succeeds");
        printed.expect("printing succeeds");

        let text = String::from_utf8(out).expect("utf-8 output");
        assert!(text.contains("[agent] start"), "got: {text}");
        assert!(text.contains("[user] !read"), "got: {text}");
        assert!(text.contains("[tool] read"), "got: {text}");
        assert!(text.contains("[tool:ok] read"), "got: {text}");
        assert!(text.contains("[assistant]"), "got: {text}");
        assert!(text.contains("[agent] end"), "got: {text}");
    }

    #[test]
    fn thinking_levels_round_trip_pi_names() {
        for name in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let level = parse_thinking_level(name).expect("known level");
            assert_eq!(thinking_level_name(level), name);
            // 大小写不敏感，和 pi 一样。
            assert_eq!(parse_thinking_level(&name.to_uppercase()), Some(level));
        }
        assert!(parse_thinking_level("nope").is_none());
    }

    #[tokio::test]
    async fn command_host_reports_and_updates_session_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let setup = crate::provider::build_from_registry(
            &crate::provider::builtin_provider_registry(dir.path()),
            "anthropic",
            Some("claude-test"),
            crate::provider::ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("provider setup");
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone())
            .with_provider_credentials([("anthropic".into(), "test-key".into())]);

        let info = session.session_info().join("\n");
        assert!(info.contains("claude-test"), "got: {info}");
        assert!(info.contains("anthropic"), "got: {info}");

        // 同一 provider 下换模型 id：真的换掉。
        let message = session.model(Some("claude-other")).expect("switch model");
        assert!(message.contains("claude-other"), "got: {message}");
        assert_eq!(session.state().model.id, "claude-other");
        assert_eq!(session.state().model.provider, "anthropic");

        // 换 provider 需要重启（本移植版的 stream service 在启动时固定）。
        assert!(session.model(Some("faux/faux")).is_err());

        // 思考级别：读回 pi 的名字。
        session.thinking(Some("high")).expect("set thinking");
        assert_eq!(thinking_level_name(session.state().thinking_level), "high");
        assert!(session.thinking(Some("nope")).is_err());
    }

    #[tokio::test]
    async fn command_host_login_configures_a_provider() {
        let dir = tempfile::tempdir().expect("temp dir");
        // 会话开始时没有 provider（CLI 无 --provider / 无环境凭据时的占位）。
        let setup = crate::provider::unconfigured_setup();
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone());
        assert_eq!(session.state().model.provider, "unconfigured");

        // `/login <provider> <api-key>` 在 TUI 里换掉 provider 服务与模型。
        let message = session
            .login(Some("anthropic sk-test"))
            .expect("login configures the provider");
        assert!(message.contains("anthropic"), "got: {message}");
        assert_eq!(session.state().model.provider, "anthropic");
        assert!(
            session.state().model.id.starts_with("claude"),
            "got: {}",
            session.state().model.id
        );

        // deepseek 复用 OpenAI 兼容实现，但 provider/api 标识独立。
        session
            .login(Some("deepseek sk-test"))
            .expect("login deepseek");
        assert_eq!(session.state().model.provider, "deepseek");
        assert_eq!(session.state().model.api, "openai-completions");
        assert_eq!(session.state().model.id, "deepseek-chat");

        // 无参数给用法；未知 provider / 缺 key 明确报错。
        assert!(
            session
                .login(None)
                .expect("usage")
                .contains("/login <provider> <api-key>")
        );
        assert!(session.login(Some("nope key")).is_err());
    }

    #[tokio::test]
    async fn command_host_login_supports_a_user_provider_plugin() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut registry = crate::provider::builtin_provider_registry(dir.path());
        registry
            .register(Arc::new(
                tower_agent::provider_builtin::OpenAiCompatibleProviderPlugin::new(
                    "local",
                    "Local",
                    "local-model",
                    None,
                )
                .with_base_url("http://127.0.0.1:8080/v1"),
            ))
            .expect("register custom provider");
        let setup = crate::provider::unconfigured_setup();
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone())
            .with_provider_registry(Arc::new(registry));

        let message = session
            .login(Some("local"))
            .expect("activate custom provider");
        assert!(message.contains("local"), "got: {message}");
        assert_eq!(session.state().model.provider, "local");
        assert_eq!(session.state().model.id, "local-model");
    }

    #[tokio::test]
    async fn command_host_persists_config_to_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let store = Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let setup = crate::provider::build_from_registry(
            &crate::provider::builtin_provider_registry(dir.path()),
            "anthropic",
            Some("claude-test"),
            crate::provider::ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("provider setup");
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone())
            .with_store(Arc::clone(&store))
            .with_provider_credentials([("anthropic".into(), "test-key".into())]);

        // `/model` / `/thinking` 写回 settings.json 的默认项。
        session.model(Some("claude-other")).expect("switch model");
        session.thinking(Some("high")).expect("set thinking");
        store.flush().await.expect("flush");

        let snapshot = store.snapshot(None).await;
        assert_eq!(snapshot.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(snapshot.default_model.as_deref(), Some("claude-other"));
        assert_eq!(snapshot.default_thinking_level.as_deref(), Some("high"));

        // `/login <provider> <api-key>` 把凭据写回 auth.json 等价物，并更新默认项。
        session
            .login(Some("deepseek sk-secret"))
            .expect("login deepseek");
        store.flush().await.expect("flush");

        let snapshot = store.snapshot(None).await;
        assert_eq!(
            snapshot.credential("deepseek").map(|c| c.api_key.as_str()),
            Some("sk-secret")
        );
        assert_eq!(snapshot.default_provider.as_deref(), Some("deepseek"));
        assert_eq!(snapshot.default_model.as_deref(), Some("deepseek-chat"));
    }

    #[cfg(feature = "codemode-python")]
    #[tokio::test]
    async fn codemode_scripts_survive_restart_from_turso() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let store = Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let first = session(dir.path()).with_store(Arc::clone(&store));
        let manager = first.mods.as_ref().expect("Python codemode host");
        let code = "\
tool_register(\"persisted_echo\", \"persisted_echo\", \"\", {\"type\": \"object\"})

def persisted_echo(args):
    return \"restored:\" + args[\"text\"]
";
        let write_tool = manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "codemode_write")
            .expect("codemode_write tool");
        let written = call_codemode_tool(
            &write_tool,
            "codemode_write",
            serde_json::json!({ "name": "restart", "code": code }),
        )
        .await;
        assert!(written.contains("restart"), "write result: {written}");
        let list_tool = manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "codemode_list")
            .expect("codemode_list tool");
        let listed = call_codemode_tool(&list_tool, "codemode_list", serde_json::json!({})).await;
        assert!(
            listed.contains("restart") && listed.contains("loaded"),
            "write must be visible to list immediately: {listed}"
        );
        drop(first);

        let second = session(dir.path()).with_store(Arc::clone(&store));
        assert_eq!(second.load_persisted_mods().await, 1);
        let tool = second
            .mods
            .as_ref()
            .expect("Python codemode host")
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "persisted_echo")
            .expect("restored script tool");
        assert_eq!(
            call_codemode_tool(&tool, "persisted_echo", serde_json::json!({ "text": "ok" })).await,
            "restored:ok"
        );

        let project = ConfigScope::from_working_dir(dir.path());
        let record = store
            .script_store()
            .get_script(&project, "restart")
            .await
            .expect("read ScriptStore")
            .expect("stored script");
        assert_eq!(record.language, ScriptLanguage::Monty.as_str());
        assert!(record.enabled);
        assert_eq!(record.source, code);
    }

    #[cfg(feature = "codemode-python")]
    #[tokio::test]
    async fn codemode_export_import_restores_and_runs_scripts() {
        let workspace = tempfile::tempdir().expect("workspace");
        let export_dir = tempfile::tempdir().expect("export dir");
        let project = ConfigScope::from_working_dir(workspace.path());
        let code = "\
tool_register(\"exported_echo\", \"exported_echo\", \"\", {\"type\": \"object\"})

def exported_echo(args):
    return \"exported:\" + args[\"text\"]
";

        let original_storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let original = Arc::new(crate::store::CliStore::from_storage(
            original_storage,
            tokio::runtime::Handle::current(),
        ));
        let writer_session = session(workspace.path()).with_store(Arc::clone(&original));
        let manager = writer_session.mods.as_ref().expect("Python codemode host");
        let write = manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "codemode_write")
            .expect("codemode_write");
        let written = call_codemode_tool(
            &write,
            "codemode_write",
            serde_json::json!({ "name": "exported", "code": code }),
        )
        .await;
        assert!(written.contains("exported"), "{written}");

        let exported = crate::codemode_transfer::export_scripts(
            original.script_store().as_ref(),
            &project,
            export_dir.path(),
        )
        .await
        .expect("export scripts");
        assert_eq!(exported, 1);
        let file = std::fs::read_dir(export_dir.path())
            .expect("export dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("codemode"))
            .expect("exported script file");
        let contents = std::fs::read_to_string(&file).expect("read export");
        assert!(contents.starts_with("--- tower-agent-codemode v1\n"));
        assert!(contents.contains("\"source_hash\""));
        assert!(contents.contains("\"permissions\""));
        assert!(contents.ends_with(code));

        // `original` is an in-memory database. Dropping it models deleting the DB.
        drop(writer_session);
        drop(original);

        let restored_storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open fresh in-memory");
        let restored = Arc::new(crate::store::CliStore::from_storage(
            restored_storage,
            tokio::runtime::Handle::current(),
        ));
        let imported = crate::codemode_transfer::import_scripts(
            restored.script_store().as_ref(),
            export_dir.path(),
            Some(&project),
        )
        .await
        .expect("import scripts");
        assert_eq!(imported, 1);

        let restored_session = session(workspace.path()).with_store(Arc::clone(&restored));
        assert_eq!(restored_session.load_persisted_mods().await, 1);
        let tool = restored_session
            .mods
            .as_ref()
            .expect("Python codemode host")
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "exported_echo")
            .expect("imported script tool");
        assert_eq!(
            call_codemode_tool(&tool, "exported_echo", serde_json::json!({ "text": "ok" })).await,
            "exported:ok"
        );

        let record = restored
            .script_store()
            .get_script(&project, "exported")
            .await
            .expect("read restored script")
            .expect("restored script exists");
        assert_eq!(record.language, ScriptLanguage::Monty.as_str());
        assert!(record.enabled);
        assert_eq!(record.source, code);
    }

    #[tokio::test]
    async fn transcript_persists_across_turns_and_resumes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let store = Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let project = crate::store::project_id(dir.path());
        let transcript = Arc::new(crate::store::Transcript::new(
            Arc::clone(&store),
            project.clone(),
            None,
            0,
        ));

        let session = Session::with_model(dir.path(), FauxProvider::new().boxed(), model())
            .with_transcript(Arc::clone(&transcript));
        session.ask("你好").await.expect("turn succeeds");
        store.flush().await.expect("flush");

        let id = transcript.id();
        let saved = store.load_transcript(&id).await;
        assert_eq!(saved.len(), session.state().messages.len());
        assert!(
            saved.len() >= 3,
            "system + user + assistant, got {}",
            saved.len()
        );

        // 会话记录归属当前项目，标题取第一条用户消息。
        let record = store
            .latest_session(&project)
            .await
            .expect("latest session");
        assert_eq!(record.id, id);
        assert_eq!(record.title.as_deref(), Some("你好"));

        // 续接：用落盘的 transcript 构造新会话，历史被恢复。
        let resumed = Session::with_history(
            dir.path(),
            FauxProvider::new().boxed(),
            model(),
            store.load_transcript(&id).await,
        );
        assert_eq!(resumed.state().messages.len(), saved.len());

        // `/new` 之后写到新的会话 id。
        session.new_session().expect("new session");
        let new_id = transcript.id();
        assert_ne!(new_id, id);
        session.ask("再来一轮").await.expect("turn succeeds");
        store.flush().await.expect("flush");
        let fresh = store.load_transcript(&new_id).await;
        assert!(fresh.len() >= 3, "new session, got {}", fresh.len());
        assert!(
            store
                .latest_session(&project)
                .await
                .is_some_and(|r| r.id == new_id)
        );
    }

    #[tokio::test]
    async fn compact_command_requests_a_compaction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let compactor = Arc::new(tower_agent_runtime::Compactor::new(
            tower_agent_runtime::CompactionSettings::default(),
        ));
        let session = Session::with_compaction(
            dir.path(),
            FauxProvider::new().boxed(),
            model(),
            Vec::new(),
            Arc::clone(&compactor),
        );

        let notice = session.compact(Some("聚焦压缩")).expect("compact");
        assert!(notice.contains("Compaction requested"), "got: {notice}");
        assert!(compactor.has_pending_request());
    }

    #[tokio::test]
    async fn transcript_persists_the_active_compaction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_plugin_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let store = Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let project = crate::store::project_id(dir.path());
        let compactor = Arc::new(tower_agent_runtime::Compactor::new(
            tower_agent_runtime::CompactionSettings::default(),
        ));
        let known = tower_agent_runtime::Compaction {
            summary: "已有摘要".into(),
            first_kept: 1,
            tokens_before: 99,
        };
        compactor.restore(known.clone());

        let transcript = Arc::new(crate::store::Transcript::new(
            Arc::clone(&store),
            project,
            None,
            0,
        ));
        let session = Session::with_compaction(
            dir.path(),
            FauxProvider::new().boxed(),
            model(),
            Vec::new(),
            Arc::clone(&compactor),
        )
        .with_transcript(Arc::clone(&transcript));
        session.ask("你好").await.expect("turn succeeds");
        store.flush().await.expect("flush");

        let record = store
            .session_record(&transcript.id())
            .await
            .expect("record");
        assert_eq!(crate::store::compaction_from_record(&record), Some(known));
    }

    #[tokio::test]
    async fn compact_command_reports_missing_compactor() {
        let dir = tempfile::tempdir().expect("temp dir");
        // 没有压缩器时明确报错，而不是静默忽略。
        let plain = Session::with_model(dir.path(), FauxProvider::new().boxed(), model());
        assert!(plain.compact(None).is_err());
    }

    #[tokio::test]
    async fn new_session_resets_the_transcript() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("你好").await.expect("turn succeeds");
        assert_eq!(
            session.state().messages.len(),
            3,
            "system + user + assistant"
        );

        session.new_session().expect("reset");
        assert_eq!(session.state().messages.len(), 1, "只剩 system 基线");
    }
}
