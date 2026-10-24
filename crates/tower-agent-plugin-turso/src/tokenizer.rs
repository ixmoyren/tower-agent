//! 应用层分词层：为 turso 的全文检索预先把文本切成「检索词元」。
//!
//! turso 的全文索引（`CREATE INDEX … USING fts`）内建的分词器只有
//! `default` / `raw` / `simple` / `whitespace` / `ngram`，**没有中文分词**。
//! 中文若直接丢给 `simple`，会被当成「每段连续的表意文字」，检索按整段匹配，
//! 几乎没有召回。解决办法是在应用层先分词，再让 turso 用 `simple` 去索引：
//!
//! ```text
//!   原始文本 ──▶ 本模块分词 ──▶ 空格拼接 ──▶ turso(`simple`) 全文索引
//!   查询串   ──▶ 本模块分词 ──▶ 空格拼接 ──▶ fts_match(tokens, 查询)
//! ```
//!
//! 索引与查询必须走**同一套**分词，否则词元对不上。分词流程：
//!
//! 1. 用 [`whichlang`] 检测整段文本的语言；
//! 2. 中文（`cmn`）用 [`jieba_rs`] 分词，其它语言用 [`unicode_segmentation`]
//!    的 Unicode 词边界。**含汉字且不含假名 / 谚文的混排文本也走 jieba**，
//!    这样「英文里夹中文」的文档与中文查询会落在同一套切分上（见 [`use_jieba`]）；
//! 3. 统一小写，去掉不含字母/数字的片段；
//! 4. 用 [`stop_words`] 去掉停用词；
//! 5. 用 [`waken_snowball`] 做词干化（仅对该语言有 Snowball 算法时）。
//!
//! 分词结果同时保留**原文**（[`Tokenized::original`]）与**词元流**
//! （[`Tokenized::tokens`]），供存储层分别写入「原始文本列」与「检索列」。

use jieba_rs::Jieba;
use unicode_segmentation::UnicodeSegmentation;
use waken_snowball::Algorithm;

pub use whichlang::Lang;

/// 检测一段文本的主导语言（[`whichlang`] 的薄封装）。
///
/// 空串或无法判断时返回 [`Lang::Eng`]（与 [`whichlang`] 行为一致）。
#[must_use]
pub fn detect_language(text: &str) -> Lang {
    whichlang::detect_language(text)
}

/// 一段文本的分词结果。
///
/// 保留原文，同时给出用于全文检索的词元流。存储层把两者分开落库：
/// 原文写原样，词元写空格拼接后的检索文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokenized {
    original: String,
    tokens: Vec<String>,
}

impl Tokenized {
    /// 未经改动的原始文本。
    #[must_use]
    pub fn original(&self) -> &str {
        &self.original
    }

    /// 切分、去停用词、词干化之后的词元流（全小写）。
    #[must_use]
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// 空格拼接后的检索文本，写进 turso 的全文索引列。
    ///
    /// 词元之间用单个空格分隔，因此 turso 的 `simple` 分词器会把每个词元
    /// 当作独立 term（中文词元也是整词，而非逐字）。
    #[must_use]
    pub fn indexed(&self) -> String {
        self.tokens.join(" ")
    }

    /// 词元数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// 是否没有任何词元（例如全是停用词）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// 应用层分词器。
///
/// 内部持有 [`Jieba`] 词典（构造时加载一次，之后只读）。同一个实例既用于
/// 写入侧分词，也用于查询侧分词，保证词元口径一致。
#[derive(Debug)]
pub struct Tokenizer {
    jieba: Jieba,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer {
    /// 新建分词器（加载 jieba 默认词典）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            jieba: Jieba::new(),
        }
    }

    /// 分词一段**待入库**的文本，保留原文。
    #[must_use]
    pub fn tokenize(&self, text: &str) -> Tokenized {
        Tokenized {
            original: text.to_owned(),
            tokens: self.token_stream(text),
        }
    }

    /// 分词一段**查询**文本，返回空格拼接的词元（喂给 `fts_match`）。
    ///
    /// 与 [`Tokenizer::tokenize`] 用同一套规则，因此查询词元能和索引词元对齐。
    #[must_use]
    pub fn tokenize_query(&self, query: &str) -> String {
        self.token_stream(query).join(" ")
    }

    /// 分词主流程：切分 → 小写 → 去噪 → 去停用词 → 词干化。
    fn token_stream(&self, text: &str) -> Vec<String> {
        let language = detect_language(text);
        let plan = Plan::for_language(language);
        let mut tokens = Vec::new();
        for word in self.segment(text, language) {
            let token = word.to_lowercase();
            if !token.chars().any(char::is_alphanumeric) {
                continue;
            }
            if plan.is_stop_word(&token) {
                continue;
            }
            tokens.push(plan.stem(token));
        }
        tokens
    }

    /// 按语言切分成原始片段（未小写、未过滤）。
    fn segment<'a>(&'a self, text: &'a str, language: Lang) -> Vec<&'a str> {
        if use_jieba(text, language) {
            self.jieba
                .cut_for_search(text, true)
                .into_iter()
                .map(|token| token.word)
                .collect()
        } else {
            text.split_word_bounds().collect()
        }
    }
}

/// 这段文本是否按中文（jieba）切分。
///
/// 规则：`whichlang` 判为中文（`cmn`）时用 jieba；此外，只要文本**含汉字且不含
/// 假名 / 谚文**（典型场景是中英文混排），也用 jieba——jieba 对 ASCII 段本就是
/// 整词切分，可让「索引侧」与「查询侧」在都含中文时保持一致，避免英文为主的
/// 文档检不到中文查询。日文 / 韩文（带假名 / 谚文）仍走 Unicode 词边界。
fn use_jieba(text: &str, language: Lang) -> bool {
    if matches!(language, Lang::Cmn) {
        return true;
    }
    let mut has_han = false;
    for ch in text.chars() {
        if is_kana(ch) || is_hangul(ch) {
            return false;
        }
        has_han |= is_han(ch);
    }
    has_han
}

/// 是否为汉字（CJK 表意文字，含基本区与扩展 A / 兼容区）。
fn is_han(ch: char) -> bool {
    matches!(ch, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// 是否为日文假名。
fn is_kana(ch: char) -> bool {
    matches!(ch, '\u{3040}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9D}')
}

/// 是否为韩文谚文。
fn is_hangul(ch: char) -> bool {
    matches!(ch, '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}' | '\u{AC00}'..='\u{D7AF}')
}

/// 一门语言的预处理参数：停用词表代码 + 词干算法。
struct Plan {
    stop_code: Option<&'static str>,
    stemmer: Option<Algorithm>,
}

impl Plan {
    /// 按检测到的语言选择停用词表与词干算法。
    ///
    /// 缺失的项（例如日语没有 Snowball 算法、某些语言没有停用词表）保持
    /// `None`，对应步骤直接跳过，不做失败处理。
    fn for_language(language: Lang) -> Self {
        let (stop_code, stemmer) = match language {
            // 中文：jieba 分词 + 中文停用词；Snowball 没有中文算法，故不做词干。
            Lang::Cmn => (Some("zh"), None),
            Lang::Eng => (Some("en"), Some(Algorithm::English)),
            Lang::Deu => (Some("de"), Some(Algorithm::German)),
            Lang::Fra => (Some("fr"), Some(Algorithm::French)),
            Lang::Spa => (Some("es"), Some(Algorithm::Spanish)),
            Lang::Ita => (Some("it"), Some(Algorithm::Italian)),
            Lang::Por => (Some("pt"), Some(Algorithm::Portuguese)),
            Lang::Nld => (Some("nl"), Some(Algorithm::Dutch)),
            Lang::Swe => (Some("sv"), Some(Algorithm::Swedish)),
            Lang::Rus => (Some("ru"), Some(Algorithm::Russian)),
            Lang::Tur => (Some("tr"), Some(Algorithm::Turkish)),
            Lang::Ara => (Some("ar"), Some(Algorithm::Arabic)),
            Lang::Hin => (Some("hi"), Some(Algorithm::Hindi)),
            // 日/韩无 Snowball 算法，靠 Unicode 词边界切分。
            Lang::Jpn => (Some("ja"), None),
            Lang::Kor => (Some("ko"), None),
            Lang::Vie => (Some("vi"), None),
        };
        Self { stop_code, stemmer }
    }

    /// 是否为停用词（比对小写形式）。
    fn is_stop_word(&self, token: &str) -> bool {
        self.stop_code
            .and_then(stop_words::lookup)
            .is_some_and(|words| words.contains(&token))
    }

    /// 词干化；没有算法时原样返回。
    fn stem(&self, token: String) -> String {
        match self.stemmer {
            Some(algorithm) => waken_snowball::stem(algorithm, &token).into_owned(),
            None => token,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_is_segmented_into_words_not_characters() {
        let tokenizer = Tokenizer::new();
        let tokenized = tokenizer.tokenize("你好，世界！");
        // 原文原样保留。
        assert_eq!(tokenized.original(), "你好，世界！");
        // jieba 把中文切成词（世界），而不是逐字。
        assert!(tokenized.tokens().contains(&"世界".to_string()));
        assert!(tokenized.tokens().iter().any(|t| t.chars().count() >= 2));
        // 标点被丢弃，检索列是空格拼接。
        assert!(
            tokenized
                .tokens()
                .iter()
                .all(|t| t.chars().all(char::is_alphanumeric))
        );
        assert_eq!(tokenized.indexed(), tokenized.tokens().join(" "));
        assert!(!tokenized.indexed().contains('，'));
    }

    #[test]
    fn english_is_lowercased_stemmed_and_stop_words_removed() {
        let tokenizer = Tokenizer::new();
        let tokenized = tokenizer.tokenize("The Runners are running quickly");
        assert_eq!(tokenized.original(), "The Runners are running quickly");
        // 停用词 the / are 被去掉，running/runners 被词干化。
        assert!(!tokenized.tokens().iter().any(|t| t == "the" || t == "are"));
        assert!(
            tokenized
                .tokens()
                .iter()
                .any(|t| t == "run" || t == "runner")
        );
    }

    #[test]
    fn mixed_text_containing_han_uses_chinese_segmentation() {
        let tokenizer = Tokenizer::new();
        // 句子以英文为主，但含中文。若按 Unicode 词边界，中文会被逐字切开
        // （全 / 文 / 检 / 索）；这里要求走 jieba，切成词而不是单字。
        let tokenized = tokenizer.tokenize("Rust makes 中文检索 feasible");
        let tokens = tokenized.tokens();
        assert!(!tokens.iter().any(|t| t == "检" || t == "索"));
        assert!(tokens.iter().any(|t| t.contains("检索")));
    }

    #[test]
    fn query_and_index_use_the_same_rule() {
        let tokenizer = Tokenizer::new();
        for text in [
            "The Runners are running quickly",
            "你好，世界！",
            "Rust makes 中文全文检索 feasible",
        ] {
            // 查询侧与索引侧必须给出完全相同的词元流，否则 FTS 命中不了。
            assert_eq!(
                tokenizer.tokenize(text).indexed(),
                tokenizer.tokenize_query(text)
            );
        }
    }

    #[test]
    fn chinese_query_terms_appear_in_chinese_index() {
        let tokenizer = Tokenizer::new();
        let indexed = tokenizer.tokenize("我们在做中文全文检索").indexed();
        for term in tokenizer.tokenize_query("中文").split(' ') {
            assert!(
                indexed.split(' ').any(|token| token == term),
                "query term `{term}` missing from indexed text `{indexed}`"
            );
        }
    }

    #[test]
    fn stop_word_only_query_yields_no_terms() {
        let tokenizer = Tokenizer::new();
        assert_eq!(tokenizer.tokenize_query("the and of"), "");
    }
}
