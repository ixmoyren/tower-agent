//! `MontyObject` / `ObjectRef` 与 `serde_json::Value` 的互转（组件侧）。

use monty_types::{MontyObject, ObjectRef};
use serde_json::Value;

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

pub fn monty_to_json(value: &MontyObject) -> Value {
    object_ref_to_json(value.as_ref())
}

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
