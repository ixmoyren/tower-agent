//! Example provider implemented as a generic tower-agent dynamic plugin.
//!
//! The plugin manifest advertises `provider.dynamic`; provider calls return
//! JSONL provider events in `AbiToolResult.content`.

#![allow(unsafe_code)]

use std::cell::RefCell;

use stabby::string::String as AbiString;
use tower_agent_plugin_sdk::{
    AbiBuffer, AbiCapability, AbiManifest, AbiToolResult, AbiVersion, HostVTable,
    declare_dynamic_plugin, provider::wire::SidecarEvent,
};

thread_local! {
    static REPLY: RefCell<tower_agent_plugin_sdk::rkyv::util::AlignedVec> =
        RefCell::new(tower_agent_plugin_sdk::rkyv::util::AlignedVec::new());
}

extern "C" fn provider_manifest() -> AbiManifest {
    AbiManifest {
        name: AbiString::from("provider.dynamic"),
        version: AbiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        },
        sdk_abi: AbiVersion::host(),
        provides: [AbiCapability {
            namespace: AbiString::from("provider"),
            name: AbiString::from("dynamic"),
            version: AbiVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
        }]
        .into_iter()
        .collect(),
    }
}

extern "C" fn provider_init(_host: *const HostVTable) -> bool {
    true
}

extern "C" fn provider_call(_request_ptr: *const u8, _request_len: usize) -> AbiBuffer {
    let event = SidecarEvent::Done {
        message: None,
        text: Some("dynamic reply".into()),
    };
    let result = match event.to_json_line() {
        Ok(content) => AbiToolResult {
            ok: true,
            content,
            details: None,
        },
        Err(error) => AbiToolResult {
            ok: false,
            content: error.to_string(),
            details: None,
        },
    };
    respond(&result)
}

extern "C" fn provider_shutdown() {}

fn respond(result: &AbiToolResult) -> AbiBuffer {
    let Ok(out) = tower_agent_plugin_sdk::rkyv::to_bytes::<
        tower_agent_plugin_sdk::rkyv::rancor::Error,
    >(result)
    else {
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

declare_dynamic_plugin!(
    provider_manifest,
    provider_init,
    provider_call,
    provider_shutdown
);

#[cfg(test)]
mod tests {
    use tower_agent_plugin_sdk::CapabilityId;

    use super::*;

    #[test]
    fn manifest_advertises_provider_capability() {
        let manifest = provider_manifest().to_manifest();
        assert_eq!(manifest.name, "provider.dynamic");
        assert!(
            manifest
                .provides_capability(&CapabilityId::new("provider", "dynamic"))
                .is_some()
        );
    }

    #[test]
    fn response_uses_the_shared_provider_wire_schema() {
        let event = SidecarEvent::Done {
            message: None,
            text: Some("dynamic reply".into()),
        };
        assert_eq!(
            event.to_json_line().expect("encode provider event"),
            "{\"type\":\"done\",\"text\":\"dynamic reply\"}\n"
        );
    }
}
