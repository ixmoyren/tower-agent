# 优雅停机

`tower-agent` 提供与传输层无关的停机协调器，可用于 Axum、Tonic、CLI 以及其他长期
运行的宿主。

```rust
use std::time::Duration;
use tower_agent::{AgentService, ShutdownCoordinator, ShutdownOptions};

let shutdown = ShutdownCoordinator::new();
let agent = AgentService::new(stream_fn)
    .with_shutdown(shutdown.clone());
```

停机生命周期如下：

```text
运行中 -> 排空中 ->（按需强制取消）-> 已停止
```

## Axum

```rust
let shutdown = ShutdownCoordinator::new();
let agent = AgentService::new(stream_fn)
    .with_shutdown(shutdown.clone());

// 用 `agent` 构造 Router 的 state。
let app = router.with_state(agent);

let signal = shutdown.clone();
tokio::spawn(async move {
    shutdown_signal().await;
    signal.begin_shutdown();
});

let drain = shutdown.clone();
let drain_task = tokio::spawn(async move {
    drain.wait_for_shutdown_start().await;
    drain
        .shutdown_with(ShutdownOptions {
            grace_period: Duration::from_secs(30),
            cancellation_timeout: Duration::from_secs(5),
        })
        .await
});

let serve = shutdown.clone();
axum::serve(listener, app)
    .with_graceful_shutdown(async move {
        serve.wait_for_shutdown_start().await;
    })
    .await?;

let report = drain_task.await?;
```

`with_graceful_shutdown` 会停止接收新的 HTTP 连接。进入排空阶段后，
`AgentService`、`AgentStreamService` 和 `AgentPool` 会拒绝新请求，并让在途请求继续
持有计数守卫，直到 agent run、transcript 保存和流式后台任务全部结束。

宽限期结束后，协调器会取消与本次停机关联的所有 agent run。provider 流、工具调用和
事件监听器都会观察到同一个取消令牌。取消超时是停机流程的硬上限。

## 插件与资源

需要在请求排空后关闭的资源可以注册为停机 Hook：

```rust
struct PluginShutdown(/* 插件管理器句柄 */);

impl tower_agent::ShutdownHook for PluginShutdown {
    fn shutdown(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            // 在这里停止或卸载动态插件与 JIT 插件。
            Ok(())
        })
    }
}

shutdown
    .register_hook("plugins", Arc::new(PluginShutdown(/* ... */)))
    .expect("停机流程尚未开始");
```

请求排空和必要的强制取消完成后，所有 Hook 会并发执行。单个 Hook 失败会记录到
`ShutdownReport::hook_errors`，不会中止其余停机步骤。

CLI 会自动注册会话停机 Hook。收到 Ctrl-C 或 SIGTERM 后，CLI 会停止接收新的 agent
工作、恢复终端、等待当前 turn 结束、关闭 JIT 与动态插件、卸载插件注册表并刷新存储。
CLI 同样使用 30 秒宽限期和 5 秒强制取消上限。

## 宿主自定义后台任务

宿主可以用 `ShutdownGuard` 跟踪其他后台任务：

```rust
let guard = shutdown.track_operation()?;
tokio::spawn(async move {
    let _guard = guard;
    background_task().await;
});
```

进入排空阶段后，新的操作守卫会以 `AgentError::ShuttingDown` 被拒绝。
