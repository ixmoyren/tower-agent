//! provider / model 元数据（pi `Model`、`BaseModel`、`ModelCost`）。

use std::collections::BTreeMap;

/// 模型输入模态（pi `"text" | "image"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputModality {
    /// 模型接受文本输入。
    Text,
    /// 模型接受图片输入。
    Image,
}

/// 每百万 token 的费率（pi `ModelCostRates`）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelCostRates {
    /// 输入 token 费率（美元 / 百万 token）。
    pub input: f64,
    /// 输出 token 费率（美元 / 百万 token）。
    pub output: f64,
    /// 缓存读取 token 费率（美元 / 百万 token）。
    pub cache_read: f64,
    /// 缓存写入 token 费率（美元 / 百万 token）。
    pub cache_write: f64,
}

/// 请求级计价档位（pi `ModelCostTier`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCostTier {
    /// 输入 token 费率（美元 / 百万 token）。
    pub input: f64,
    /// 输出 token 费率（美元 / 百万 token）。
    pub output: f64,
    /// 缓存读取 token 费率（美元 / 百万 token）。
    pub cache_read: f64,
    /// 缓存写入 token 费率（美元 / 百万 token）。
    pub cache_write: f64,
    /// 当一次请求的总输入用量超过该 token 数时使用本档位。
    pub input_tokens_above: u64,
}

/// 模型计价（pi `ModelCost`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelCost {
    /// 输入 token 费率（美元 / 百万 token）。
    pub input: f64,
    /// 输出 token 费率（美元 / 百万 token）。
    pub output: f64,
    /// 缓存读取 token 费率（美元 / 百万 token）。
    pub cache_read: f64,
    /// 缓存写入 token 费率（美元 / 百万 token）。
    pub cache_write: f64,
    /// 请求级计价档位；命中阈值最高的一档作用于整个请求。
    pub tiers: Vec<ModelCostTier>,
}

/// 可用于 `stream` / `streamSimple` 的 chat 模型（pi `Model`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    /// 模型标识（pi `Model.id`）。
    pub id: String,
    /// 人类可读的模型名（pi `Model.name`）。
    pub name: String,
    /// 具体 API 标识（pi `Api`，如 `"anthropic-messages"`）。
    pub api: String,
    /// provider 标识（pi `ProviderId`）。
    pub provider: String,
    /// API base URL。
    pub base_url: String,
    /// 该模型接受的输入模态。
    pub input: Vec<InputModality>,
    /// 该模型是否支持 reasoning/thinking。
    pub reasoning: bool,
    /// 上下文窗口大小（token）。
    pub context_window: u64,
    /// 单次响应的最大输出 token 数。
    pub max_tokens: u64,
    /// 该模型的计价信息。
    pub cost: ModelCost,
    /// 该模型自带的默认请求头。
    pub headers: BTreeMap<String, String>,
}
