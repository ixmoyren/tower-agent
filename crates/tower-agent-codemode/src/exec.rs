//! 命令层：把 guest 的 `exec(name, args, opts)` 落成「起一份 coreutils 组件实例、内存 stdio、
//! 拿回 code/stdout/stderr」。
//!
//! 三条硬约束（都踩过，见 `docs/codemode.md` §6）：
//! 1. **`argv[0]` 是裸命令名**（`cat`），组件靠它分发，`uucore::util_name()` 也读它；
//! 2. **argv 必须包含 `argv[0]`**，否则 clap 会把第一个真实参数当程序名吃掉；
//! 3. **会话根要映射成 guest 的 `/`**：uucore 的 `canonicalize` 会逐级 `stat` 前缀，
//!    只 preopen 子目录时 `mv` 会直接 `ENOENT`（`cp` 却正常）。

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tower_agent_wasmtime::{HostState, SandboxEngine, SandboxOptions};
use wasmtime::{Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    I32Exit, ResourceTable, WasiCtxBuilder,
    p2::{
        bindings::Command,
        pipe::{MemoryInputPipe, MemoryOutputPipe},
    },
};

use crate::{
    CodemodeError,
    component::tower_agent::codemode::host::{ExecOpts, ExecResult},
};

/// 单次命令的 stdout/stderr 上限（管道容量；写满后 guest 侧看到写失败）。
const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;

/// 进程内共享的**命令引擎**。
///
/// coreutils 组件 7.4 MB，编译一次实测 ~10 s，而实例化只要几毫秒——所以引擎要跨 host
/// 复用。会话根与挂载是 per-`Store` 的（在 `exec` 里才用），因此不参与这是缓存键。
static COMMAND_ENGINES: Mutex<Vec<(u64, Arc<SandboxEngine>)>> = Mutex::new(Vec::new());

/// 取（或编译并缓存）命令引擎。键 = wasm 内容 + 单命令超时。
fn shared_engine(
    wasm: &[u8],
    command_timeout: std::time::Duration,
    options: SandboxOptions,
) -> Result<Arc<SandboxEngine>, CodemodeError> {
    let mut hasher = DefaultHasher::new();
    wasm.hash(&mut hasher);
    command_timeout.as_millis().hash(&mut hasher);
    let key = hasher.finish();

    let mut cache = COMMAND_ENGINES
        .lock()
        .map_err(|_| CodemodeError::Worker("命令引擎缓存中毒".to_owned()))?;
    if let Some((_, engine)) = cache.iter().find(|(cached, _)| *cached == key) {
        return Ok(Arc::clone(engine));
    }
    let engine = Arc::new(SandboxEngine::new(wasm, options)?);
    cache.push((key, Arc::clone(&engine)));
    Ok(engine)
}

/// 「命令即将开始」的回调类型（给 [`crate::CodemodeTool`] 推进度用）。
pub(crate) type ProgressSink = Box<dyn FnMut(&str, &[String])>;

thread_local! {
    /// 当前线程的进度回调。
    ///
    /// `exec` 是同步 host import，跑在调用 `run` 的线程上，所以 `thread_local` 足够，
    /// 也不需要在 `ExecService` 里放共享状态。
    static PROGRESS: std::cell::RefCell<Option<ProgressSink>> = const { std::cell::RefCell::new(None) };
}

/// 设置（或清除）当前线程的进度回调。
pub(crate) fn set_progress_sink(sink: Option<ProgressSink>) {
    PROGRESS.with(|slot| *slot.borrow_mut() = sink);
}

/// 通知一次「命令即将开始」。
fn notify_progress(name: &str, args: &[String]) {
    PROGRESS.with(|slot| {
        if let Some(sink) = slot.borrow_mut().as_mut() {
            sink(name, args);
        }
    });
}

/// 命令层的后端：一份共享的 coreutils 组件 + 一个会话根目录。
///
/// 所有命令实例共享同一个编译产物（`SandboxEngine`），但每次执行都开一个新的 `Store`——
/// 也就是「组件编译一次、实例每次新建」。trap 只污染那一次的 `Store`，天然不需要「丢弃实例」
/// 的额外机制（池化留给 M5）。
pub struct ExecService {
    engine: Arc<SandboxEngine>,
    root: PathBuf,
    mounts: Vec<crate::Mount>,
}

impl ExecService {
    /// 用 coreutils 组件字节与会话根构造。`root` 会被映射成 guest 的 `/`。
    pub fn new(
        coreutils_wasm: &[u8],
        root: impl Into<PathBuf>,
        mounts: &[crate::Mount],
        command_timeout: std::time::Duration,
        options: &SandboxOptions,
    ) -> Result<Self, CodemodeError> {
        // 命令侧开 epoch 中断：一次 `exec` 超时就 trap，而不是一直占着线程。
        let options = SandboxOptions {
            epoch_timeout: Some(command_timeout),
            ..options.clone()
        };
        Ok(Self {
            engine: shared_engine(coreutils_wasm, command_timeout, options)?,
            root: root.into(),
            mounts: mounts.to_vec(),
        })
    }

    /// 会话根。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 跑一条命令。`name` 是裸命令名（`cat`）。
    ///
    /// 时限到了会被**取消**（异步链路 + `tokio::time::timeout`）——这是唯一能掐断
    /// 「卡在宿主调用里」的手段（epoch 只能打断正在执行的 wasm 指令）。
    pub async fn exec(
        &self,
        name: &str,
        args: &[String],
        opts: &ExecOpts,
    ) -> Result<ExecResult, String> {
        notify_progress(name, args);

        // argv = [命令名, 参数…] —— 第 0 个元素就是组件要的 `argv[0]`。
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(name.to_owned());
        argv.extend(args.iter().cloned());

        let stdin = MemoryInputPipe::new(opts.stdin.clone().unwrap_or_default());
        let stdout = MemoryOutputPipe::new(OUTPUT_CAPACITY);
        let stderr = MemoryOutputPipe::new(OUTPUT_CAPACITY);

        let mut builder = WasiCtxBuilder::new();
        builder.args(&argv);
        builder.stdin(stdin);
        builder.stdout(stdout.clone());
        builder.stderr(stderr.clone());
        // 与 JS 侧共用同一套挂载：会话根可写，额外挂载按各自的模式（默认只读）。
        crate::preopen(&mut builder, &self.root, &self.mounts).map_err(|e| e.to_string())?;
        let ctx = builder.build();

        let mut store = Store::new(
            self.engine.engine(),
            HostState {
                ctx,
                table: ResourceTable::new(),
                limits: self.limits(),
            },
        );
        store.limiter(|state| &mut state.limits);
        // 按次覆盖默认时限（`exec` 的 `timeoutMs`）。
        self.engine.arm_deadline(
            &mut store,
            opts.timeout_ms
                .map(|ms| std::time::Duration::from_millis(u64::from(ms))),
        );

        let command =
            Command::instantiate_async(&mut store, self.engine.component(), self.engine.linker())
                .await
                .map_err(|e| format!("instantiate: {e}"))?;

        let deadline = std::time::Duration::from_millis(u64::from(
            opts.timeout_ms
                .unwrap_or(crate::DEFAULT_COMMAND_TIMEOUT.as_millis() as u32),
        ));
        let outcome =
            tokio::time::timeout(deadline, command.wasi_cli_run().call_run(&mut store)).await;

        let code = match outcome {
            // 取消发生在 await 点：future 被丢弃、`store` 随即销毁，不留悬挂线程。
            Err(_elapsed) => {
                return Err(format!("{name}: 命令超时（{} ms）", deadline.as_millis()));
            }
            Ok(Ok(Ok(()))) => 0,
            Ok(Ok(Err(()))) => 1,
            // 组件显式 `wasi:cli/exit` 会以 `I32Exit` 出来，那是一次正常退出。
            Ok(Err(error)) => match error.downcast_ref::<I32Exit>() {
                Some(exit) => exit.0,
                None => return Err(format!("{error}")),
            },
        };

        Ok(ExecResult {
            code,
            stdout: stdout.contents().to_vec(),
            stderr: stderr.contents().to_vec(),
        })
    }

    fn limits(&self) -> StoreLimits {
        StoreLimitsBuilder::new()
            .memory_size(self.engine.memory_limit())
            .build()
    }
}
