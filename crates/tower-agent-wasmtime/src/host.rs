//! `Store` 的宿主状态：WASI 上下文、资源表与 wasm 内存上限。

use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// 沙箱实例的宿主状态。
///
/// WASI 上下文**不挂任何 preopen**，因此 wasm 侧没有宿主文件系统直通；想要文件访问由
/// 上层（如 monty 的挂载表）自己为具体能力实现。
pub struct HostState {
    /// WASI 上下文。
    pub ctx: WasiCtx,
    /// WASI 资源表。
    pub table: ResourceTable,
    /// wasm 线性内存上限（由 `Store::limiter` 应用）。
    pub limits: StoreLimits,
}

impl HostState {
    /// 按内存上限构造。
    #[must_use]
    pub fn new(memory_limit: usize) -> Self {
        Self {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(memory_limit).build(),
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
