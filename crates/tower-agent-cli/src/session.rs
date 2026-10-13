//! 会话入口：把 agent、内置工具与（可选的）TUI 拼成一个可运行的会话。
//!
//! 一层的职责划分：
//!
//! - [`Session`] 通过 [`crate::stack`] 用 `ServiceBuilder` 装配出类型擦除的
//!   `BoxAgentService`（跑 turn）与 `BoxAgentControl`（订阅事件、切模型/工具、取消、重置），
//!   自身不直接持有 `Agent` 具体类型；内置工具由 `tower_agent_plugins` 装配；
//! - 事件通过 [`Session::subscribe`] 拿到的 `tower_agent_runtime::Subscription` 流出，
//!   既可以交给 [`tower_agent_tui::run_agent_tui`] 渲染，也可以用 [`run_print`] 打成纯文本（便于脚本与测试）；
//! - provider 由调用方注入（[`Session::new`] 的 `stream` 参数）；测试用确定性的离线
//!   provider [`tower_agent_provider::provider::faux::FauxProvider`]，二进制则接真实 provider。

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use tower::ServiceExt as _;
use tower_agent_core::UiState;
#[cfg(feature = "mods")]
use tower_agent_monty::ModManager;
#[cfg(feature = "codemode")]
use tower_agent_plugins::StaticPlugin as _;
use tower_agent_storage::{ConfigScope, ProviderCredential};

use crate::store::CliStore;
use tower_agent_provider::{
    BoxStreamService, Message, Model, ModelThinkingLevel, UserContent, UserMessage,
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
    /// mods 宿主；解释器内嵌在 `tower-agent-monty` 中，默认启用（`TPA_NO_MODS` 可关闭）。
    #[cfg(feature = "mods")]
    mods: Option<Arc<ModManager>>,
    /// 工具过滤（`--tools` / `--exclude-tools` / `--no-tools`）。
    tool_filter: Mutex<ToolFilter>,
    /// 配置存储：`/login` `/model` `/thinking` 会把结果写回（对齐 pi 的 `auth.json` /
    /// `settings.json`）；未接入存储时为 `None`。
    store: Option<Arc<CliStore>>,
    /// transcript 持久化句柄（会话按项目归属）；未落盘时为 `None`。
    transcript: Option<Arc<crate::store::Transcript>>,
    /// 上下文压缩器（核心能力，由 agent 循环在每次请求前自动执行）；未启用时为 `None`。
    compactor: Option<Arc<Compactor>>,
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
    /// `<config>/tower-agent/codemode/<name>`（跨进程复用），否则用临时目录（会话即焚）。
    ///
    /// 启用 mods 时把 codemode 作为**受保护静态插件**注册进统一注册表（名字 `codemode`、
    /// 能力 `tool.codemode` 因此进 `protected_*`，动态 / JIT 插件不能顶替）；未启用 mods
    /// 时退化成直接追加到地基工具集。
    #[cfg(feature = "codemode")]
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
        let plugin = tower_agent_codemode::CodemodePlugin::new(workspace, root).with_mounts(parsed);
        #[cfg(feature = "mods")]
        if let Some(mods) = &self.mods {
            mods.register_static_plugin(Arc::new(plugin))
                .map_err(|error| error.to_string())?;
            self.refresh_tools();
            return Ok(());
        }
        let mut context = tower_agent_plugins::PluginContext::new();
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
        Self::build(cwd, stream, model, messages, None)
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
        Self::build(cwd, stream, model, messages, Some(compactor))
    }

    fn build(
        cwd: impl AsRef<Path>,
        stream: BoxStreamService,
        model: Model,
        messages: Vec<AgentMessage>,
        compactor: Option<Arc<Compactor>>,
    ) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        let system_prompt = format!(
            "你是 tower-agent（pi 的 Rust 移植版）的编码助手。可用工具：{}。\n\
         使用工具而不是猜测文件内容；修改文件前先读一遍。",
            tower_agent_plugins::DEFAULT_TOOL_NAMES.join(", ")
        );
        let base_tools = tower_agent_plugins::default_tools(&cwd);
        // mods（cc `mods` 模式）：LLM 生成 monty 代码当插件，可提供工具、改 UI。
        // 内置静态插件与 JIT mod 共用一张注册表：工具目录来自「解析 + 安装」。
        // monty 解释器随 `tower-agent-monty` 内嵌（CLI feature `mods`），`TPA_NO_MODS` 可关闭。
        #[cfg(feature = "mods")]
        let mods = if std::env::var_os("TPA_NO_MODS").is_none() {
            // 插件集合与 mods 宿主都来自组装层（含 wasmtime 等受保护宿主能力），
            // 不在会话里硬编码。
            match tower_agent::assembly::HostPlugins::new(&cwd).mods(UiState::new()) {
                Ok(manager) => {
                    // 把工作目录只读挂到 /work，供 mod 读取项目文件；写操作被拒。
                    let _ = manager.mount_readonly("/work", &cwd);
                    Some(manager)
                }
                Err(error) => {
                    eprintln!("mods 初始化失败（本次会话已关闭 mods）：{error}");
                    None
                }
            }
        } else {
            None
        };

        #[cfg(feature = "mods")]
        let initial_tools = match &mods {
            Some(mods) => mods.tools(),
            None => base_tools.clone(),
        };
        #[cfg(not(feature = "mods"))]
        let initial_tools = base_tools.clone();

        let mut options = AgentOptions::new(stream);
        options.initial.tools = initial_tools;
        options.initial.model = Some(model);
        options.initial.system_prompt = Some(system_prompt);
        options.initial.messages = messages;
        options.compactor = compactor.clone();
        let (service, control) = crate::stack::assemble(options);
        Self {
            service: Mutex::new(service),
            control,
            cwd,
            base_tools: Mutex::new(base_tools),
            #[cfg(feature = "mods")]
            mods,
            tool_filter: Mutex::new(ToolFilter::default()),
            store: None,
            transcript: None,
            compactor,
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
        self.store = Some(store);
        self
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
        #[cfg(feature = "mods")]
        {
            self.mods.as_ref().map(|mods| mods.ui())
        }
        #[cfg(not(feature = "mods"))]
        {
            None
        }
    }

    /// 后台预热 mods 沙箱（编译组件 + 预建实例）；在运行时内调用才生效。
    pub fn prewarm_mods(&self) {
        #[cfg(feature = "mods")]
        if let Some(mods) = &self.mods {
            mods.spawn_warmup();
        }
    }

    /// 加载持久化的 mod（`dirs` 的 config 目录）；返回加载数量。
    pub async fn load_persisted_mods(&self) -> usize {
        #[cfg(feature = "mods")]
        {
            match &self.mods {
                Some(mods) => mods.load_persisted().await,
                None => 0,
            }
        }
        #[cfg(not(feature = "mods"))]
        {
            0
        }
    }

    /// 从 `dirs` 的 config 目录（`tower-agent/plugins`）加载动态插件；返回加载数量。
    #[allow(unsafe_code)] // 加载用户配置目录里的动态库，视作受信任。
    pub fn load_dynamic_plugins(&self) -> usize {
        #[cfg(feature = "mods")]
        {
            let Some(mods) = &self.mods else {
                return 0;
            };
            match unsafe { mods.load_dynamic_plugins_from_config() } {
                Ok(names) => names.len(),
                Err(error) => {
                    eprintln!("加载动态插件失败：{error}");
                    0
                }
            }
        }
        #[cfg(not(feature = "mods"))]
        {
            0
        }
    }

    /// 重新计算工具集：目录（静态 + JIT 的统一解析结果）经过滤 + mods 作者工具。
    ///
    /// mod 在运行中 `tool_register` 之后，下一轮 turn 开始前会走到这里，因此新工具
    /// 会被 agent 看到。
    fn refresh_tools(&self) {
        #[cfg(feature = "mods")]
        let catalog = match &self.mods {
            Some(mods) => mods.tools(),
            None => self.base_tools.lock().expect("base tools poisoned").clone(),
        };
        #[cfg(not(feature = "mods"))]
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

/// 地基作者工具名（`mod_write` 等）：不受 `--tools` 过滤影响，否则就没法写 mod 了。
///
/// 没有编译进 mods 能力时不存在作者工具，一律不算保留名。
fn is_host_reserved(name: &str) -> bool {
    #[cfg(feature = "mods")]
    {
        tower_agent_monty::is_reserved_name(name)
    }
    #[cfg(not(feature = "mods"))]
    {
        let _ = name;
        false
    }
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

/// 用户可选的 provider 列表（`/model` 的错误提示用；不含测试用的 faux）。
#[must_use]
pub fn available_providers() -> String {
    crate::provider::ProviderKind::ALL
        .iter()
        .map(|kind| kind.canonical_name())
        .collect::<Vec<_>>()
        .join(" / ")
}

impl tower_agent_tui::CommandHost for Session {
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
                "current model: {} (provider {} / api {})\nusage: /model <provider>/<id> or /model <id>",
                current.id, current.provider, current.api
            ));
        };

        let (requested, id) = match arg.split_once('/') {
            Some((provider, id)) => (Some(provider.trim()), id.trim()),
            None => (None, arg.trim()),
        };
        if id.is_empty() {
            return Err("model id must not be empty. usage: /model <provider>/<id>".into());
        }

        // 没写 `provider/` 时：先看它是不是 provider 名（用它的默认模型），否则按当前
        // provider 的模型 id 处理。
        let kind = match requested {
            Some(provider) => crate::provider::ProviderKind::parse(provider).ok_or_else(|| {
                format!(
                    "unknown provider: {provider} (available: {})",
                    available_providers()
                )
            })?,
            None => match crate::provider::ProviderKind::parse(id) {
                Some(kind) => kind,
                None => crate::provider::ProviderKind::ALL
                    .into_iter()
                    // 先按 (provider, api) 精确匹配（openai 与 deepseek 共用
                    // `openai-completions`）；再退回只按 provider 标识匹配。
                    .find(|kind| kind.id() == current.provider && kind.api() == current.api)
                    .or_else(|| {
                        crate::provider::ProviderKind::ALL
                            .into_iter()
                            .find(|kind| kind.id() == current.provider)
                    })
                    .ok_or_else(|| {
                        format!(
                            "cannot determine the current provider ({})",
                            current.provider
                        )
                    })?,
            },
        };
        let model_id = match requested {
            Some(_) => id,
            None => {
                if crate::provider::ProviderKind::parse(id).is_some() {
                    kind.default_model_id()
                } else {
                    id
                }
            }
        };

        if kind.id() != current.provider {
            return Err(format!(
                "this port fixes the provider at startup (currently {}); restart with \
                 `--provider` to switch.",
                current.provider
            ));
        }

        let model = crate::provider::model_for(kind, model_id);
        self.set_model(model.clone());
        // 写回 pi `settings.json` 的 `defaultProvider` / `defaultModel`。
        self.persist_config(
            crate::store::DEFAULT_PROVIDER,
            serde_json::json!(model.provider),
        );
        self.persist_config(crate::store::DEFAULT_MODEL, serde_json::json!(model.id));
        Ok(format!(
            "model switched to: {} (provider {})",
            model.id, model.provider
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
                available_providers()
            ));
        };
        let mut parts = arg.splitn(2, char::is_whitespace);
        let provider = parts.next().unwrap_or_default().trim();
        let api_key = parts.next().map(str::trim).filter(|key| !key.is_empty());
        let kind = crate::provider::ProviderKind::parse(provider).ok_or_else(|| {
            format!(
                "unknown provider: {provider} (available: {})",
                available_providers()
            )
        })?;

        // 没带 key 时退回环境变量；两条路都缺凭据就报错。
        let options = crate::provider::ProviderStackOptions::default();
        let setup = match api_key {
            Some(api_key) => {
                crate::provider::build_with_api_key(kind, None, options, Some(api_key))
            }
            None => crate::provider::build(kind, None, options),
        }
        .map_err(|error| error.to_string())?;

        self.control.set_stream(setup.stream_fn());
        self.control.set_model(setup.model.clone());
        // 对齐 pi：`/login` 把凭据写进 `auth.json`、默认 provider / 模型写进 `settings.json`。
        if let Some(store) = &self.store {
            let model_id = setup.model.id.clone();
            // 只持久化显式给出的 key；`/login <provider>`（回退到环境变量）不把环境里的 key 落盘。
            if let Some(api_key) = api_key {
                store.queue_put_credential(ProviderCredential {
                    provider: kind.id().to_string(),
                    api_key: api_key.to_string(),
                    model: Some(model_id.clone()),
                    base_url: None,
                    updated_at: tower_agent_runtime::agent_loop::now_ms(),
                });
            }
            store.queue_set_config(
                ConfigScope::User,
                crate::store::DEFAULT_PROVIDER,
                serde_json::json!(kind.id()),
            );
            store.queue_set_config(
                ConfigScope::User,
                crate::store::DEFAULT_MODEL,
                serde_json::json!(model_id),
            );
        }
        Ok(format!(
            "provider configured: {} (model {})",
            kind.canonical_name(),
            setup.model.id
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
                writeln!(out, "[user] {}", tower_agent_tui::render_message(message))
            }
            tower_agent_runtime::AgentMessage::Message(Message::Assistant(_)) => {
                writeln!(
                    out,
                    "[assistant] {}",
                    tower_agent_tui::render_message(message)
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
            tower_agent_tui::render_message(&tower_agent_runtime::AgentMessage::Message(
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
    session.load_dynamic_plugins();
    session.prewarm_mods();
    let subscription = session.subscribe();
    let (submissions_tx, mut submissions_rx) = mpsc::unbounded_channel::<String>();
    // 斜杠命令（`/new`、`/model`…）在 TUI 里执行，通过这个 trait 回调到会话。
    let host: Arc<dyn tower_agent_tui::CommandHost> = session.clone();

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
    #[cfg(feature = "mods")]
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
        provider::faux::{FauxProvider, model},
    };
    use tower_agent_tui::CommandHost as _;

    use super::{Session, parse_thinking_level, run_print, thinking_level_name};

    fn session(cwd: &std::path::Path) -> Session {
        Session::with_model(cwd, FauxProvider::new().boxed(), model())
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
                ) => Some(tower_agent_tui::render_message(
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
            tower_agent_tui::render_message(&tower_agent_runtime::AgentMessage::Message(
                tower_agent_provider::Message::ToolResult(tool_result.clone())
            )),
            "工具读到的内容\n"
        );
        assert!(last_assistant_text(&state).contains("工具结果已收到"));
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
            tower_agent_tui::render_message(&tower_agent_runtime::AgentMessage::Message(
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
            names.starts_with(&tower_agent_plugins::DEFAULT_TOOL_NAMES),
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
        let setup = crate::provider::build_with_api_key(
            crate::provider::ProviderKind::Anthropic,
            Some("claude-test"),
            crate::provider::ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("provider setup");
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone());

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
    async fn command_host_persists_config_to_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_turso::TursoStorage::in_memory()
            .await
            .expect("open in-memory");
        let store = Arc::new(crate::store::CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let setup = crate::provider::build_with_api_key(
            crate::provider::ProviderKind::Anthropic,
            Some("claude-test"),
            crate::provider::ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("provider setup");
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone())
            .with_store(Arc::clone(&store));

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

    #[tokio::test]
    async fn transcript_persists_across_turns_and_resumes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let storage = tower_agent_turso::TursoStorage::in_memory()
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
        let storage = tower_agent_turso::TursoStorage::in_memory()
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
