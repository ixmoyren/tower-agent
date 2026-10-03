//! 消息与工具载荷共用的 JSON 别名。

/// 任意 JSON 值（pi `JsonValue`）。
pub type JsonValue = serde_json::Value;

/// JSON 对象（pi `JsonObject`）。
pub type JsonObject = serde_json::Map<String, JsonValue>;
