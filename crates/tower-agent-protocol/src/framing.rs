//! 4 字节大端长度前缀的帧编解码（对应 pi `packages/protocol/src/framing.ts`）。

/// 帧头的字节数。
pub const FRAME_HEADER_LENGTH: usize = 4;
/// 无符号 32 位上界。
pub const MAX_UINT32: usize = 0xffff_ffff;
/// 单个帧载荷的默认上限（16MB）。
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

/// 帧编解码错误（pi `FrameError`）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FrameError {
    message: String,
}

impl FrameError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误信息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 帧解码器的选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDecoderOptions {
    /// 单个帧的最大字节数，默认 [`DEFAULT_MAX_FRAME_LENGTH`]。
    pub max_frame_length: Option<usize>,
}

impl FrameDecoderOptions {
    fn resolve(self) -> Result<usize, FrameError> {
        let value = self.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        if value > MAX_UINT32 {
            return Err(FrameError::new(format!(
                "maxFrameLength must be an integer between 0 and {MAX_UINT32}"
            )));
        }
        Ok(value)
    }
}

/// 给载荷加上 4 字节大端长度前缀（pi `encodeFrame`）。
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_UINT32 {
        return Err(FrameError::new(
            "Frame payload exceeds the unsigned 32-bit length limit",
        ));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Open,
    Ended,
    Failed,
}

/// 增量式帧解码器：把任意切分的字节块切成完整的载荷。
///
/// 与 pi 一致：允许零长度帧，超过上限或流在帧中途结束时失败，失败/结束后再次使用会报错。
#[derive(Debug)]
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_length: usize,
    max_frame_length: usize,
    payload: Vec<u8>,
    expected_payload_length: Option<usize>,
    state: DecoderState,
}

impl FrameDecoder {
    /// 用给定选项构造。
    pub fn new(options: FrameDecoderOptions) -> Result<Self, FrameError> {
        Ok(Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_length: 0,
            max_frame_length: options.resolve()?,
            payload: Vec::new(),
            expected_payload_length: None,
            state: DecoderState::Open,
        })
    }

    /// 推入一段字节，返回这次凑齐的所有载荷。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        match self.state {
            DecoderState::Ended => return Err(FrameError::new("Frame decoder has ended")),
            DecoderState::Failed => return Err(FrameError::new("Frame decoder has failed")),
            DecoderState::Open => {}
        }

        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            if self.expected_payload_length.is_none() {
                let missing = FRAME_HEADER_LENGTH - self.header_length;
                let available = chunk.len() - offset;
                let take = missing.min(available);
                self.header[self.header_length..self.header_length + take]
                    .copy_from_slice(&chunk[offset..offset + take]);
                self.header_length += take;
                offset += take;
                if self.header_length < FRAME_HEADER_LENGTH {
                    continue;
                }

                let length = u32::from_be_bytes(self.header) as usize;
                self.header_length = 0;
                if length > self.max_frame_length {
                    return Err(self.fail(format!(
                        "Frame length {length} exceeds configured limit of {}",
                        self.max_frame_length
                    )));
                }
                if length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                self.expected_payload_length = Some(length);
                self.payload.clear();
                self.payload.reserve(length.min(64 * 1024));
            }

            let Some(expected) = self.expected_payload_length else {
                continue;
            };
            let remaining = expected - self.payload.len();
            let available = chunk.len() - offset;
            let take = remaining.min(available);
            self.payload
                .extend_from_slice(&chunk[offset..offset + take]);
            offset += take;

            if self.payload.len() == expected {
                frames.push(std::mem::take(&mut self.payload));
                self.expected_payload_length = None;
            }
        }
        Ok(frames)
    }

    /// 结束解码；此时还有半截帧就报错。
    pub fn end(&mut self) -> Result<(), FrameError> {
        match self.state {
            DecoderState::Ended => return Err(FrameError::new("Frame decoder has ended")),
            DecoderState::Failed => return Err(FrameError::new("Frame decoder has failed")),
            DecoderState::Open => {}
        }
        if self.header_length != 0 || self.expected_payload_length.is_some() {
            return Err(self.fail("Truncated frame at end of stream"));
        }
        self.state = DecoderState::Ended;
        Ok(())
    }

    fn fail(&mut self, message: impl Into<String>) -> FrameError {
        self.state = DecoderState::Failed;
        self.header_length = 0;
        self.payload.clear();
        self.expected_payload_length = None;
        FrameError::new(message)
    }
}
