//! `MontyObject` / `ObjectRef` 与 `serde_json::Value` 的互转。
//!
//! `MontyObject` 内部是 arena 图，直接 `serde` 会序列化它的存储布局而不是值本身，
//! 因此这里按 Python 值语义做显式转换：`None/bool/int/float/str/list/tuple/dict`
//! 映射到对应 JSON 类型，其余（类实例、字节、日期等）退化为 `py_repr()` 字符串。

use monty_types::{MontyObject, ObjectRef};
use serde_json::Value;

/// 把宿主可见的值转成 JSON。
#[must_use]
pub fn object_ref_to_json(value: ObjectRef<'_>) -> Value {
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

/// 把 [`MontyObject`] 转成 JSON。
#[must_use]
pub fn monty_to_json(value: &MontyObject) -> Value {
    object_ref_to_json(value.as_ref())
}

/// 把 JSON 转成宿主喂给沙箱的 [`MontyObject`]。
#[must_use]
pub fn json_to_monty(value: &Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(flag) => MontyObject::bool(*flag),
        Value::Number(number) => number.as_i64().map_or_else(
            || MontyObject::float(number.as_f64().unwrap_or_default()),
            MontyObject::int,
        ),
        Value::String(text) => MontyObject::string(text.clone()),
        Value::Array(items) => MontyObject::list(items.iter().map(json_to_monty)),
        Value::Object(object) => MontyObject::dict(
            object
                .iter()
                .map(|(key, value)| (MontyObject::string(key.clone()), json_to_monty(value))),
        ),
    }
}

/// 把 JSON 渲染成等价的 Python 字面量（用于把参数拼进调用表达式）。
///
/// JSON 的字符串/数组/对象语法与 Python 兼容；只需把 `true/false/null` 换成
/// `True/False/None`。
#[must_use]
pub fn python_literal(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_literal).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(object) => {
            let inner: Vec<String> = object
                .iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!("{key}: {}", python_literal(value))
                })
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use monty_types::MontyObject;
    use serde_json::json;

    use super::*;

    #[test]
    fn python_literal_matches_json_for_scalars_and_containers() {
        assert_eq!(python_literal(&json!(null)), "None");
        assert_eq!(python_literal(&json!(true)), "True");
        assert_eq!(python_literal(&json!(3)), "3");
        assert_eq!(python_literal(&json!("hi")), "\"hi\"");
        assert_eq!(python_literal(&json!([1, null])), "[1, None]");
        assert_eq!(python_literal(&json!({"a": 1})), "{\"a\": 1}");
    }

    #[test]
    fn json_round_trips_through_monty_objects() {
        let value = json!({"name": "world", "n": 3, "ok": true, "items": [1, 2]});
        let object = json_to_monty(&value);
        let object_ref = object.as_ref();
        assert_eq!(object_ref.pairs().unwrap().len(), 4);
        let back = monty_to_json(&object);
        assert_eq!(back, value, "dict order is preserved and values survive");
    }

    #[test]
    fn strings_are_not_confused_with_ints() {
        assert_eq!(monty_to_json(&MontyObject::string("12")), json!("12"));
        assert_eq!(monty_to_json(&MontyObject::int(12)), json!(12));
    }
}
