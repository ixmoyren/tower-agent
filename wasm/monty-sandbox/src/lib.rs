//! Wasm 组件沙箱：在组件内部运行 monty 解释器。
//!
//! 宿主驱动模型：`start` 喂源码并返回首个 `progress`；挂起在外部函数调用时，
//! 宿主算完结果后调 `resume-function` 继续。组件内部用一个 `thread_local` 保存
//! 当前挂起的 REPL 进度（每个组件实例 = 一个 mod 会话，天然隔离）。

#[allow(warnings)]
mod bindings;

mod json;

use std::cell::RefCell;

use bindings::exports::tower_agent::monty::sandbox::{FunctionCall, Guest, OsCall, Progress};
use monty::{MontyRepl, ReplProgress, ReplStartError};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, NameLookupResult, NamedValues,
    PrintWriter, ResourceLimits, ResourceTracker,
};

/// 组件的导出实现。
struct Component;

thread_local! {
    static STATE: RefCell<Option<ReplProgress>> = const { RefCell::new(None) };
}

/// 脚本名（出现在 traceback 里）。
const SCRIPT_NAME: &str = "mod.py";

/// 沙箱资源上限：单次 feed/turn 的执行时长、递归深度与挂起次数。
///
/// 没有这个时长上限，`while True: pass` 会一直占住宿主（进程内解释器的默认值
/// `max_feed_duration = None`）。
fn resource_limits() -> ResourceLimits {
    ResourceLimits {
        max_feed_duration: Some(std::time::Duration::from_secs(5)),
        max_turn_duration: Some(std::time::Duration::from_secs(2)),
        max_recursion_depth: 200,
        max_suspensions: 10_000,
        ..ResourceLimits::default()
    }
}

/// 存入进度并生成给宿主看的视图。
fn store(progress: ReplProgress) -> Progress {
    let view = view_of(&progress);
    STATE.with(|state| *state.borrow_mut() = Some(progress));
    view
}

/// 把一次 resume 的结果转成宿主视图。
fn finish(result: Result<ReplProgress, Box<ReplStartError>>) -> Progress {
    match result {
        Ok(progress) => store(progress),
        Err(error) => Progress::Error(error.error.to_string()),
    }
}

/// 从 `STATE` 取出当前挂起进度。
fn take_state() -> Option<ReplProgress> {
    STATE.with(|state| state.borrow_mut().take())
}

/// 生成进度视图（借用，不改持有状态）。
fn view_of(progress: &ReplProgress) -> Progress {
    match progress {
        ReplProgress::Complete { value, .. } => {
            Progress::Complete(json::monty_to_json(value).to_string())
        }
        ReplProgress::FunctionCall(call) => Progress::FunctionCall(FunctionCall {
            function_name: call.function_name.clone(),
            args_json: serde_json::Value::Array(
                call.args.args().map(json::object_ref_to_json).collect(),
            )
            .to_string(),
            kwargs_json: serde_json::Value::Object(
                call.args
                    .kwargs()
                    .map(|(key, value)| {
                        let key = key.as_str().map_or_else(|| key.py_repr(), str::to_string);
                        (key, json::object_ref_to_json(value))
                    })
                    .collect(),
            )
            .to_string(),
            call_id: call.call_id,
        }),
        ReplProgress::NameLookup(lookup) => Progress::NameLookup(lookup.name.clone()),
        ReplProgress::OsCall(call) => Progress::OsCall(OsCall {
            call_id: call.call_id,
            payload_json: serde_json::to_string(&call.function_call).unwrap_or_default(),
        }),
        ReplProgress::ResolveFutures(_) => {
            Progress::Error("async sandbox futures are not supported".to_string())
        }
    }
}

impl Guest for Component {
    fn start(source: String) -> Progress {
        let repl = MontyRepl::new(
            SCRIPT_NAME,
            ResourceTracker::new(resource_limits()),
            CompileOptions::default(),
        );
        match repl.feed_start(&source, NamedValues::new(), PrintWriter::Disabled) {
            Ok(progress) => store(progress),
            Err(error) => Progress::Error(error.error.to_string()),
        }
    }

    fn resume_function(call_id: u32, result_json: String) -> Progress {
        let Some(ReplProgress::FunctionCall(call)) = take_state() else {
            return Progress::Error("no suspended function call".to_string());
        };
        if call.call_id != call_id {
            STATE.with(|state| *state.borrow_mut() = Some(ReplProgress::FunctionCall(call)));
            return Progress::Error("function call id mismatch".to_string());
        }
        let resume = match serde_json::from_str::<serde_json::Value>(&result_json) {
            Ok(value) => ExtFunctionResult::Return(json::json_to_monty(&value)),
            Err(error) => ExtFunctionResult::Error(MontyException::new(
                ExcType::RuntimeError,
                Some(error.to_string()),
            )),
        };
        finish(call.resume(resume, PrintWriter::Disabled))
    }

    fn reject_function(call_id: u32, message: String) -> Progress {
        let Some(ReplProgress::FunctionCall(call)) = take_state() else {
            return Progress::Error("no suspended function call".to_string());
        };
        if call.call_id != call_id {
            STATE.with(|state| *state.borrow_mut() = Some(ReplProgress::FunctionCall(call)));
            return Progress::Error("function call id mismatch".to_string());
        }
        let exception = MontyException::new(ExcType::RuntimeError, Some(message));
        finish(call.resume(ExtFunctionResult::Error(exception), PrintWriter::Disabled))
    }

    fn resume_name(name: String) -> Progress {
        let _ = name;
        match take_state() {
            Some(ReplProgress::NameLookup(lookup)) => {
                finish(lookup.resume(NameLookupResult::Undefined, PrintWriter::Disabled))
            }
            other => {
                STATE.with(|state| *state.borrow_mut() = other);
                Progress::Error("no suspended name lookup".to_string())
            }
        }
    }

    fn resume_os(call_id: u32, result_json: String) -> Progress {
        let Some(ReplProgress::OsCall(call)) = take_state() else {
            return Progress::Error("no suspended os call".to_string());
        };
        if call.call_id != call_id {
            STATE.with(|state| *state.borrow_mut() = Some(ReplProgress::OsCall(call)));
            return Progress::Error("os call id mismatch".to_string());
        }
        let result = match serde_json::from_str::<OsResult>(&result_json) {
            Ok(OsResult::Return { value }) => {
                ExtFunctionResult::Return(json::json_to_monty(&value))
            }
            Ok(OsResult::Error { exception }) => ExtFunctionResult::Error(exception),
            Ok(OsResult::NotHandled) => {
                ExtFunctionResult::Error(call.function_call.on_no_handler())
            }
            Err(error) => ExtFunctionResult::Error(MontyException::new(
                ExcType::RuntimeError,
                Some(format!("invalid os-call result: {error}")),
            )),
        };
        finish(call.resume(result, PrintWriter::Disabled))
    }
}

/// 宿主回传的 OS 调用结果。
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OsResult {
    /// 宿主处理成功，携带结果值（JSON）。
    Return { value: serde_json::Value },
    /// 宿主处理失败，携带要抛出的异常。
    Error { exception: MontyException },
    /// 没有处理器：由沙箱自己决定默认异常。
    NotHandled,
}

bindings::export!(Component with_types_in bindings);
