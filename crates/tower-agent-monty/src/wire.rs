//! os-call 结果的 JSON 编码（宿主 -> 沙箱）。
//!
//! 沙箱把 OS 调用序列化成 `OsFunctionCall` JSON 交给宿主；宿主服务后按下面三种
//! 形状回传：
//!
//! - `{"kind":"return","value":<json>}`：成功，`value` 是结果的 JSON 视图；
//! - `{"kind":"error","exception":<MontyException>}`：抛出宿主给的异常；
//! - `{"kind":"not_handled"}`：没有处理器，由沙箱决定默认异常。

use monty_types::{MontyException, MontyObject, ObjectRef};
use serde_json::{Value, json};

/// 「没有处理器」结果。
pub const NOT_HANDLED: &str = r#"{"kind":"not_handled"}"#;

/// 成功结果。
#[must_use]
pub fn return_result(value: &MontyObject) -> Value {
    json!({ "kind": "return", "value": monty_to_json(value) })
}

/// 错误结果（抛出给定异常）。
#[must_use]
pub fn error_result(exception: MontyException) -> Value {
    json!({ "kind": "error", "exception": exception })
}

/// 用消息构造一个 `PermissionError` 结果。
#[must_use]
pub fn permission_denied(message: impl Into<String>) -> Value {
    error_result(MontyException::new(
        monty_types::ExcType::PermissionError,
        Some(message.into()),
    ))
}

/// `MontyObject` -> JSON（按 Python 值语义）。
#[must_use]
pub fn monty_to_json(value: &MontyObject) -> Value {
    object_ref_to_json(value.as_ref())
}

fn object_ref_to_json(value: ObjectRef<'_>) -> Value {
    if let Some(text) = value.as_str() {
        return Value::String(text.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Value::Bool(flag);
    }
    if let Some(int) = value.as_int() {
        return Value::from(int);
    }
    if let Some(float) = value.as_float() {
        return Value::from(float);
    }
    if value.type_name() == "NoneType" {
        return Value::Null;
    }
    if let Some(items) = value.items() {
        return Value::Array(items.into_iter().map(object_ref_to_json).collect());
    }
    if let Some(pairs) = value.pairs() {
        let mut object = serde_json::Map::new();
        for (key, item) in pairs {
            let key = key.as_str().map_or_else(|| key.py_repr(), str::to_string);
            object.insert(key, object_ref_to_json(item));
        }
        return Value::Object(object);
    }
    Value::String(value.py_repr())
}
