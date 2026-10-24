//! Dynamic `tool.shell` plugin.
//!
//! The public model declaration and the synchronous dynamic-plugin ABI live
//! here. The shell implementation owns a dedicated Tokio runtime and shares
//! the persistent brush session across calls.

mod shell;

use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
};

use stabby::{string::String as AbiString, vec::Vec as AbiVec};
use tower::Service;
use tower_agent_plugin_sdk::{
    AbiBuffer, AbiCapability, AbiManifest, AbiToolCall, AbiToolDescriptor, AbiToolResult, AbiValue,
    AbiVersion, ArchivedAbiToolCall, HostVTable, declare_dynamic_plugin, rkyv,
};
use tower_agent_provider::{JsonValue, ToolCall};
use tower_agent_runtime::ToolCallRequest;

use crate::shell::ShellTool;

const PLUGIN_NAME: &str = "shell";
const PLUGIN_VERSION: AbiVersion = AbiVersion {
    major: 1,
    minor: 0,
    patch: 0,
};

thread_local! {
    static REPLY: RefCell<rkyv::util::AlignedVec> = RefCell::new(rkyv::util::AlignedVec::new());
}

static WORKER: OnceLock<Mutex<Option<Worker>>> = OnceLock::new();

struct Worker {
    requests: SyncSender<WorkerRequest>,
    thread: Option<JoinHandle<()>>,
}

enum WorkerRequest {
    Run {
        call: AbiToolCall,
        reply: SyncSender<AbiToolResult>,
    },
    Shutdown,
}

extern "C" fn shell_manifest() -> AbiManifest {
    AbiManifest {
        name: AbiString::from(PLUGIN_NAME),
        version: PLUGIN_VERSION,
        sdk_abi: AbiVersion::host(),
        provides: [AbiCapability {
            namespace: AbiString::from("tool"),
            name: AbiString::from(ShellTool::NAME),
            version: PLUGIN_VERSION,
        }]
        .into_iter()
        .collect(),
        tool_descriptors: shell_tool_descriptors(),
    }
}

extern "C" fn shell_init(_host: *const HostVTable) -> bool {
    let worker = WORKER.get_or_init(|| Mutex::new(None));
    let mut slot = worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return true;
    }
    match spawn_worker() {
        Ok(worker) => {
            *slot = Some(worker);
            true
        }
        Err(_) => false,
    }
}

extern "C" fn shell_call(request_ptr: *const u8, request_len: usize) -> AbiBuffer {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if request_ptr.is_null() {
            return AbiToolResult {
                ok: false,
                content: "empty shell request".to_owned(),
                details: None,
            };
        }
        let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let call = match rkyv::access::<ArchivedAbiToolCall, rkyv::rancor::Error>(bytes) {
            Ok(call) => call,
            Err(error) => {
                return AbiToolResult {
                    ok: false,
                    content: format!("invalid dynamic shell request: {error}"),
                    details: None,
                };
            }
        };
        let call = AbiToolCall {
            id: call.id.as_str().to_owned(),
            tool: call.tool.as_str().to_owned(),
            args: AbiValue::from_archived(&call.args),
        };
        dispatch(call).unwrap_or_else(|error| AbiToolResult {
            ok: false,
            content: error,
            details: None,
        })
    }))
    .unwrap_or_else(|_| AbiToolResult {
        ok: false,
        content: "dynamic shell plugin panicked".to_owned(),
        details: None,
    });
    respond(&result)
}

extern "C" fn shell_shutdown() {
    let Some(worker) = WORKER.get() else {
        return;
    };
    let mut slot = worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(mut worker) = slot.take() {
        let _ = worker.requests.send(WorkerRequest::Shutdown);
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
    }
}

fn shell_tool_descriptors() -> AbiVec<AbiToolDescriptor> {
    [AbiToolDescriptor {
        name: AbiString::from(ShellTool::NAME),
        description: AbiString::from(ShellTool::description_text()),
        parameters_json: AbiString::from(ShellTool::parameters().to_string()),
        output_schema_json: AbiString::from(shell::shell_output_schema().to_string()),
    }]
    .into_iter()
    .collect()
}

fn dispatch(call: AbiToolCall) -> Result<AbiToolResult, String> {
    let worker = WORKER
        .get()
        .ok_or_else(|| "dynamic shell plugin is not initialized".to_owned())?
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let worker = worker
        .as_ref()
        .ok_or_else(|| "dynamic shell worker is unavailable".to_owned())?;
    let (reply, response) = mpsc::sync_channel(1);
    worker
        .requests
        .send(WorkerRequest::Run { call, reply })
        .map_err(|_| "dynamic shell worker stopped".to_owned())?;
    response
        .recv()
        .map_err(|_| "dynamic shell worker stopped before replying".to_owned())
}

fn spawn_worker() -> Result<Worker, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (requests, receiver) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("tower-agent-shell-plugin".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            let shell = ShellTool::new(cwd);
            while let Ok(request) = receiver.recv() {
                let WorkerRequest::Run { call, reply } = request else {
                    break;
                };
                let result = runtime.block_on(run_shell(&shell, call));
                let _ = reply.send(result);
            }
        })
        .map_err(|error| format!("failed to start dynamic shell worker: {error}"))?;
    Ok(Worker {
        requests,
        thread: Some(thread),
    })
}

async fn run_shell(shell: &ShellTool, call: AbiToolCall) -> AbiToolResult {
    let args = call.args.to_json();
    let arguments = match args {
        JsonValue::Object(map) => map.into(),
        other => {
            return AbiToolResult {
                ok: false,
                content: format!("shell arguments must be an object, got {other}"),
                details: None,
            };
        }
    };
    let request = ToolCallRequest {
        tool_call: ToolCall {
            id: call.id,
            name: call.tool,
            arguments,
            thought_signature: None,
            namespace: None,
        },
        signal: None,
    };
    let mut service = shell.clone();
    let mut stream = match service.call(request).await {
        Ok(stream) => stream,
        Err(error) => {
            return AbiToolResult {
                ok: false,
                content: error.to_string(),
                details: None,
            };
        }
    };
    let mut final_result = None;
    use futures_util::StreamExt as _;
    while let Some(result) = stream.next().await {
        final_result = Some(result);
    }
    let Some(result) = final_result else {
        return AbiToolResult {
            ok: false,
            content: "dynamic shell returned no result".to_owned(),
            details: None,
        };
    };
    let content = result
        .content
        .iter()
        .filter_map(|block| match block {
            tower_agent_provider::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    AbiToolResult {
        ok: !result.is_error,
        content,
        details: result.details.as_ref().map(AbiValue::from_json),
    }
}

fn respond(result: &AbiToolResult) -> AbiBuffer {
    let Ok(out) = rkyv::to_bytes::<rkyv::rancor::Error>(result) else {
        return AbiBuffer::empty();
    };
    REPLY.with(|cell| {
        *cell.borrow_mut() = out;
        let bytes = cell.borrow();
        AbiBuffer {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    })
}

declare_dynamic_plugin!(shell_manifest, shell_init, shell_call, shell_shutdown);

#[cfg(test)]
mod tests {
    use tower_agent_plugin_sdk::CapabilityId;

    use super::*;

    fn call(command: &str) -> AbiToolCall {
        AbiToolCall {
            id: "shell-test".to_owned(),
            tool: "shell".to_owned(),
            args: AbiValue::from_json(&serde_json::json!({ "command": command })),
        }
    }

    #[test]
    fn manifest_and_tool_metadata_are_complete() {
        let manifest = shell_manifest().to_manifest();
        assert_eq!(manifest.name, "shell");
        assert!(
            manifest
                .provides_capability(&CapabilityId::new("tool", "shell"))
                .is_some()
        );
        let descriptors = &manifest.tool_descriptors;
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].name, "shell");
        let parameters: JsonValue =
            serde_json::from_str(&descriptors[0].parameters_json).expect("valid schema");
        assert_eq!(parameters["required"][0], "command");
    }

    #[tokio::test]
    async fn shell_session_persists_and_reports_exit_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shell = ShellTool::new(dir.path());

        let result = run_shell(&shell, call("export GREETING=hi")).await;
        assert!(result.ok, "{}", result.content);
        let result = run_shell(&shell, call("printf '%s\\n' \"$GREETING\"")).await;
        assert!(result.ok, "{}", result.content);
        assert_eq!(result.content.trim(), "hi");

        let result = run_shell(&shell, call("exit 7")).await;
        assert!(!result.ok);
        assert!(result.content.contains("Command exited with code 7"));
    }
}
