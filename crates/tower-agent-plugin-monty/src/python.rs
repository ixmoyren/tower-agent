//! JSON -> Python 字面量。
//!
//! mod 工具函数按 `def <fn>(args): ...` 约定接收一个 dict，宿主把工具参数拼成
//! 调用表达式 `fn({...})` 传给沙箱。JSON 的字符串/数组/对象语法与 Python 兼容，
//! 只需把 `true/false/null` 换成 `True/False/None`。

use serde_json::Value;

/// 把 JSON 渲染成等价的 Python 字面量。
#[must_use]
pub fn literal(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(literal).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(object) => {
            let inner: Vec<String> = object
                .iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!("{key}: {}", literal(value))
                })
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn literals_match_python() {
        assert_eq!(literal(&json!(null)), "None");
        assert_eq!(literal(&json!(true)), "True");
        assert_eq!(literal(&json!(3)), "3");
        assert_eq!(literal(&json!("hi")), "\"hi\"");
        assert_eq!(literal(&json!([1, null])), "[1, None]");
        assert_eq!(literal(&json!({"a": 1})), "{\"a\": 1}");
    }
}
