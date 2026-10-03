//! SSE（`text/event-stream`）解析（对应 pi anthropic provider 里的 `iterateSseMessages`）。
//!
//! 只实现 provider 实际需要的部分：`event:` 与 `data:` 字段（多行 `data` 用 `\n` 连接）、
//! 注释行忽略、空行派发；`\r\n` 与 `\n` 都接受。

/// 一个完整的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 字段（缺省时沿用上一个事件名，与 SSE 规范一致）。
    pub event: Option<String>,
    /// `data:` 字段拼起来的内容。
    pub data: String,
}

/// 增量 SSE 解析器：字节块进，事件出。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    /// 推入一段（已 UTF-8 解码的）文本，返回这次凑齐的事件。
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let mut line = self.buffer[..index].to_string();
            self.buffer.drain(..=index);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(event) = self.consume_line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// 结束解析：处理残留的半个行与未派发的事件。
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.consume_line(&line) {
                events.push(event);
            }
        }
        if let Some(event) = self.flush() {
            events.push(event);
        }
        events.into_iter().next()
    }

    fn consume_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            // 注释/心跳。
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let data = self.data.join("\n");
        let event = self.event.take();
        self.data.clear();
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push("event: message_start\ndata: {\"a\"")
                .is_empty()
        );
        let events = decoder.push(":1}\n\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message_start".into()),
                data: "{\"a\":1}".into(),
            }]
        );
    }

    #[test]
    fn joins_multiple_data_lines_and_ignores_comments() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(": ping\ndata: one\ndata: two\r\n\r\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "one\ntwo".into(),
            }]
        );
    }

    #[test]
    fn flushes_a_trailing_event_without_a_blank_line() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("data: tail").is_empty());
        assert_eq!(
            decoder.finish(),
            Some(SseEvent {
                event: None,
                data: "tail".into(),
            })
        );
    }
}
