//! pi protocol 用的**严格 CBOR 子集**（对应 pi `packages/protocol/src/cbor`）。
//!
//! 只接受 RFC 8949 的 definite-length 子集：整数用最短编码、浮点只支持 64 位、
//! 不支持 tag / indefinite-length / break、map 键必须是字符串且不得重复。
//! 编解码两侧都施加字节数、容器长度与嵌套深度上限，默认值与 pi 一致。
//!
//! 数值按 JavaScript 的安全整数范围校验（`|v| <= 2^53-1`），因为 pi 解码后是 JS number：
//! 超出范围的整数在 pi 侧同样会被拒绝，这里保持一致。

use serde_json::{Map as JsonObject, Number as JsonNumber, Value as JsonValue};

/// 2^32，用于 8 字节参数的高低位拆解。
pub const UINT32_BASE: u64 = 0x1_0000_0000;
/// 无符号 32 位上界。
pub const MAX_UINT32: u64 = 0xffff_ffff;
/// JavaScript 安全整数上界（2^53 - 1）。
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_CONFIGURED_DEPTH: usize = 512;

/// 单个 CBOR 载荷的默认字节上限（16MB）。
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
/// 数组元素 / map 条目的默认上限。
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
/// 默认嵌套深度上限。
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;

/// CBOR 编解码错误（pi `CborError`）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CborError {
    message: String,
}

impl CborError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误信息（与 pi 的文案一致，便于对拍）。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 调用方可以覆盖的上限。
#[derive(Debug, Clone, Copy, Default)]
pub struct CborOptions {
    /// 编码输出/解码输入的最大字节数，同时也是字符串与字节串的长度上限。
    pub max_byte_length: Option<usize>,
    /// 数组元素 / map 条目的最大数量。
    pub max_container_length: Option<usize>,
    /// 最大嵌套深度。
    pub max_depth: Option<usize>,
}

/// 解析后的上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCborOptions {
    /// 见 [`CborOptions::max_byte_length`]。
    pub max_byte_length: usize,
    /// 见 [`CborOptions::max_container_length`]。
    pub max_container_length: usize,
    /// 见 [`CborOptions::max_depth`]。
    pub max_depth: usize,
}

impl CborOptions {
    /// 应用默认值并校验范围（pi `resolveOptions`）。
    pub fn resolve(self) -> Result<ResolvedCborOptions, CborError> {
        Ok(ResolvedCborOptions {
            max_byte_length: resolve_limit(
                "maxByteLength",
                self.max_byte_length.unwrap_or(DEFAULT_MAX_CBOR_BYTE_LENGTH),
                MAX_UINT32 as usize,
            )?,
            max_container_length: resolve_limit(
                "maxContainerLength",
                self.max_container_length
                    .unwrap_or(DEFAULT_MAX_CBOR_CONTAINER_LENGTH),
                MAX_UINT32 as usize,
            )?,
            max_depth: resolve_limit(
                "maxDepth",
                self.max_depth.unwrap_or(DEFAULT_MAX_CBOR_DEPTH),
                MAX_CONFIGURED_DEPTH,
            )?,
        })
    }
}

fn resolve_limit(name: &str, value: usize, maximum: usize) -> Result<usize, CborError> {
    if value > maximum {
        return Err(CborError::new(format!(
            "{name} must be an integer between 0 and {maximum}"
        )));
    }
    Ok(value)
}

/// CBOR 数据模型（对应 pi 编解码接受的 JS 值集合）。
///
/// map 用有序键值对表示：既能保留插入顺序（与 JS 对象一致），又能在解码时检测重复键。
#[derive(Debug, Clone, PartialEq)]
pub enum CborValue {
    /// `null`
    Null,
    /// 布尔
    Bool(bool),
    /// 非负整数
    Uint(u64),
    /// 负整数
    Int(i64),
    /// 64 位浮点
    Float(f64),
    /// 文本字符串
    Text(String),
    /// 字节串
    Bytes(Vec<u8>),
    /// 数组
    Array(Vec<CborValue>),
    /// map（键必须是字符串）
    Map(Vec<(String, CborValue)>),
}

impl CborValue {
    /// 从 JSON 值构造（JSON 是 CBOR 子集的子集）。
    pub fn from_json(value: &JsonValue) -> Self {
        match value {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(value) => Self::Bool(*value),
            JsonValue::Number(number) => {
                if let Some(value) = number.as_u64() {
                    Self::Uint(value)
                } else if let Some(value) = number.as_i64() {
                    Self::Int(value)
                } else {
                    Self::Float(number.as_f64().unwrap_or_default())
                }
            }
            JsonValue::String(text) => Self::Text(text.clone()),
            JsonValue::Array(items) => Self::Array(items.iter().map(Self::from_json).collect()),
            JsonValue::Object(entries) => Self::Map(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_json(value)))
                    .collect(),
            ),
        }
    }

    /// 转成 JSON 值；字节串不是 JSON 值，会报错（对应 pi 的 `isJsonValue` 检查）。
    pub fn to_json(&self) -> Result<JsonValue, CborError> {
        Ok(match self {
            Self::Null => JsonValue::Null,
            Self::Bool(value) => JsonValue::Bool(*value),
            Self::Uint(value) => JsonValue::Number(JsonNumber::from(*value)),
            Self::Int(value) => JsonValue::Number(JsonNumber::from(*value)),
            Self::Float(value) => JsonNumber::from_f64(*value)
                .map(JsonValue::Number)
                .ok_or_else(|| CborError::new("CBOR numbers must be finite"))?,
            Self::Text(text) => JsonValue::String(text.clone()),
            Self::Bytes(_) => {
                return Err(CborError::new("CBOR byte strings are not JSON values"));
            }
            Self::Array(items) => JsonValue::Array(
                items
                    .iter()
                    .map(Self::to_json)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Self::Map(entries) => {
                let mut object = JsonObject::new();
                for (key, value) in entries {
                    object.insert(key.clone(), value.to_json()?);
                }
                JsonValue::Object(object)
            }
        })
    }
}

/// 把 CBOR 值编码成字节（pi `encodeCbor`）。
pub fn encode_cbor(value: &CborValue, options: CborOptions) -> Result<Vec<u8>, CborError> {
    let resolved = options.resolve()?;
    let mut writer = Writer::new(resolved.max_byte_length);
    write_value(&mut writer, value, resolved, 0)?;
    Ok(writer.finish())
}

/// 解码恰好一个 CBOR 值（pi `decodeCbor`）。
pub fn decode_cbor(bytes: &[u8], options: CborOptions) -> Result<CborValue, CborError> {
    let resolved = options.resolve()?;
    if bytes.len() > resolved.max_byte_length {
        return Err(CborError::new(format!(
            "CBOR byte length exceeds configured limit of {}",
            resolved.max_byte_length
        )));
    }
    let mut reader = Reader::new(bytes, resolved);
    let value = reader.read_item(0)?;
    if reader.offset != bytes.len() {
        return Err(CborError::new("CBOR payload contains trailing data"));
    }
    Ok(value)
}

struct Writer {
    buffer: Vec<u8>,
    max_byte_length: usize,
}

impl Writer {
    fn new(max_byte_length: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_byte_length,
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buffer
    }

    fn ensure_capacity(&self, additional: usize) -> Result<(), CborError> {
        if self.buffer.len() + additional > self.max_byte_length {
            return Err(CborError::new(format!(
                "CBOR byte length exceeds configured limit of {}",
                self.max_byte_length
            )));
        }
        Ok(())
    }

    fn byte(&mut self, value: u8) -> Result<(), CborError> {
        self.ensure_capacity(1)?;
        self.buffer.push(value);
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), CborError> {
        self.ensure_capacity(value.len())?;
        self.buffer.extend_from_slice(value);
        Ok(())
    }
}

fn write_argument(writer: &mut Writer, major_type: u8, value: u64) -> Result<(), CborError> {
    let prefix = major_type << 5;
    if value < 24 {
        writer.byte(prefix | value as u8)?;
    } else if value <= 0xff {
        writer.byte(prefix | 24)?;
        writer.byte(value as u8)?;
    } else if value <= 0xffff {
        writer.byte(prefix | 25)?;
        writer.bytes(&(value as u16).to_be_bytes())?;
    } else if value <= MAX_UINT32 {
        writer.byte(prefix | 26)?;
        writer.bytes(&(value as u32).to_be_bytes())?;
    } else {
        writer.byte(prefix | 27)?;
        writer.bytes(&value.to_be_bytes())?;
    }
    Ok(())
}

fn write_text(
    writer: &mut Writer,
    text: &str,
    options: ResolvedCborOptions,
) -> Result<(), CborError> {
    if text.len() > options.max_byte_length {
        return Err(CborError::new(format!(
            "CBOR text string length exceeds configured limit of {}",
            options.max_byte_length
        )));
    }
    write_argument(writer, 3, text.len() as u64)?;
    writer.bytes(text.as_bytes())
}

fn write_value(
    writer: &mut Writer,
    value: &CborValue,
    options: ResolvedCborOptions,
    depth: usize,
) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError::new(format!(
            "CBOR nesting depth exceeds configured limit of {}",
            options.max_depth
        )));
    }
    match value {
        CborValue::Null => writer.byte(0xf6),
        CborValue::Bool(false) => writer.byte(0xf4),
        CborValue::Bool(true) => writer.byte(0xf5),
        CborValue::Uint(number) => {
            if *number > MAX_SAFE_INTEGER {
                return Err(CborError::new(
                    "CBOR integers must be safe JavaScript integers",
                ));
            }
            write_argument(writer, 0, *number)
        }
        CborValue::Int(number) => {
            let magnitude = number.unsigned_abs();
            if magnitude > MAX_SAFE_INTEGER {
                return Err(CborError::new(
                    "CBOR integers must be safe JavaScript integers",
                ));
            }
            write_argument(writer, 1, magnitude - 1)
        }
        CborValue::Float(number) => {
            if !number.is_finite() {
                return Err(CborError::new("CBOR numbers must be finite"));
            }
            writer.byte(0xfb)?;
            writer.bytes(&number.to_be_bytes())
        }
        CborValue::Text(text) => write_text(writer, text, options),
        CborValue::Bytes(bytes) => {
            if bytes.len() > options.max_byte_length {
                return Err(CborError::new(format!(
                    "CBOR byte string length exceeds configured limit of {}",
                    options.max_byte_length
                )));
            }
            write_argument(writer, 2, bytes.len() as u64)?;
            writer.bytes(bytes)
        }
        CborValue::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR array length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            write_argument(writer, 4, items.len() as u64)?;
            for item in items {
                write_value(writer, item, options, depth + 1)?;
            }
            Ok(())
        }
        CborValue::Map(entries) => {
            if entries.len() > options.max_container_length {
                return Err(CborError::new(format!(
                    "CBOR map length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            write_argument(writer, 5, entries.len() as u64)?;
            for (key, entry) in entries {
                write_text(writer, key, options)?;
                write_value(writer, entry, options, depth + 1)?;
            }
            Ok(())
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: ResolvedCborOptions,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], options: ResolvedCborOptions) -> Self {
        Self {
            bytes,
            offset: 0,
            options,
        }
    }

    fn read_item(&mut self, depth: usize) -> Result<CborValue, CborError> {
        if depth > self.options.max_depth {
            return Err(CborError::new(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                self.options.max_depth
            )));
        }
        let initial = self.read_byte()?;
        let major_type = initial >> 5;
        let additional = initial & 0x1f;
        match major_type {
            0 => {
                let value = self.read_argument(additional)?;
                if value > MAX_SAFE_INTEGER {
                    return Err(CborError::new(
                        "Decoded CBOR integer is outside the safe range",
                    ));
                }
                Ok(CborValue::Uint(value))
            }
            1 => {
                let value = self.read_argument(additional)?;
                let negative = -1 - value as i128;
                if negative < -(MAX_SAFE_INTEGER as i128) {
                    return Err(CborError::new(
                        "Decoded CBOR integer is outside the safe range",
                    ));
                }
                Ok(CborValue::Int(negative as i64))
            }
            2 => {
                let length =
                    self.read_length(additional, "byte string", self.options.max_byte_length)?;
                Ok(CborValue::Bytes(self.read_bytes(length)?.to_vec()))
            }
            3 => {
                let length =
                    self.read_length(additional, "text string", self.options.max_byte_length)?;
                let bytes = self.read_bytes(length)?;
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| CborError::new("CBOR text string contains invalid UTF-8"))?;
                Ok(CborValue::Text(text.to_string()))
            }
            4 => {
                let length =
                    self.read_length(additional, "array", self.options.max_container_length)?;
                let mut items = Vec::with_capacity(length.min(1024));
                for _ in 0..length {
                    items.push(self.read_item(depth + 1)?);
                }
                Ok(CborValue::Array(items))
            }
            5 => {
                let length =
                    self.read_length(additional, "map", self.options.max_container_length)?;
                let mut entries: Vec<(String, CborValue)> = Vec::with_capacity(length.min(1024));
                for _ in 0..length {
                    let key = match self.read_item(depth + 1)? {
                        CborValue::Text(text) => text,
                        _ => return Err(CborError::new("CBOR map keys must be strings")),
                    };
                    if entries.iter().any(|(existing, _)| *existing == key) {
                        return Err(CborError::new("CBOR map contains a duplicate key"));
                    }
                    let value = self.read_item(depth + 1)?;
                    entries.push((key, value));
                }
                Ok(CborValue::Map(entries))
            }
            6 => Err(CborError::new("CBOR tags are not supported")),
            7 => self.read_simple(additional),
            _ => Err(CborError::new("Malformed CBOR major type")),
        }
    }

    fn read_simple(&mut self, additional: u8) -> Result<CborValue, CborError> {
        match additional {
            20 => Ok(CborValue::Bool(false)),
            21 => Ok(CborValue::Bool(true)),
            22 => Ok(CborValue::Null),
            27 => {
                let bytes = self.read_bytes(8)?;
                let value = f64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                if !value.is_finite() {
                    return Err(CborError::new("Decoded CBOR number must be finite"));
                }
                if value.fract() == 0.0 && value.abs() > MAX_SAFE_INTEGER as f64 {
                    return Err(CborError::new(
                        "Decoded CBOR integer is outside the safe range",
                    ));
                }
                Ok(CborValue::Float(value))
            }
            31 => Err(CborError::new("CBOR break marker is not supported")),
            _ => Err(CborError::new(
                "Unsupported CBOR simple value or floating-point width",
            )),
        }
    }

    fn read_length(
        &mut self,
        additional: u8,
        kind: &str,
        limit: usize,
    ) -> Result<usize, CborError> {
        if additional == 31 {
            return Err(CborError::new(format!(
                "Indefinite-length CBOR {kind}s are not supported"
            )));
        }
        let length = self.read_argument(additional)?;
        if length > limit as u64 {
            return Err(CborError::new(format!(
                "CBOR {kind} length exceeds configured limit of {limit}"
            )));
        }
        Ok(length as usize)
    }

    fn read_argument(&mut self, additional: u8) -> Result<u64, CborError> {
        if additional < 24 {
            return Ok(u64::from(additional));
        }
        match additional {
            24 => Ok(u64::from(self.read_byte()?)),
            25 => {
                let bytes = self.read_bytes(2)?;
                Ok(u64::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            }
            26 => {
                let bytes = self.read_bytes(4)?;
                Ok(u64::from(u32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ])))
            }
            27 => {
                let bytes = self.read_bytes(8)?;
                Ok(u64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))
            }
            31 => Err(CborError::new(
                "Indefinite-length CBOR items are not supported",
            )),
            _ => Err(CborError::new("Malformed CBOR additional information")),
        }
    }

    fn read_byte(&mut self) -> Result<u8, CborError> {
        if self.offset >= self.bytes.len() {
            return Err(CborError::new("Truncated CBOR payload"));
        }
        let value = self.bytes[self.offset];
        self.offset += 1;
        Ok(value)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], CborError> {
        if length > self.bytes.len().saturating_sub(self.offset) {
            return Err(CborError::new("Truncated CBOR payload"));
        }
        let value = &self.bytes[self.offset..self.offset + length];
        self.offset += length;
        Ok(value)
    }
}
