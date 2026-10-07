//! 帧编解码测试（对应 pi `test/framing.test.ts`）。

use tower_agent_protocol::{FrameDecoder, FrameDecoderOptions, encode_frame};

#[test]
fn prefixes_payloads_with_a_four_byte_big_endian_length() {
    assert_eq!(
        encode_frame(&[0xaa, 0xbb, 0xcc]).expect("encode"),
        vec![0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc]
    );
    assert_eq!(encode_frame(&[]).expect("encode"), vec![0, 0, 0, 0]);
}

#[test]
fn decodes_fragmented_coalesced_and_empty_frames_in_order() {
    let mut wire = Vec::new();
    wire.extend(encode_frame(&[1, 2, 3]).expect("encode"));
    wire.extend(encode_frame(&[]).expect("encode"));
    wire.extend(encode_frame(&[4]).expect("encode"));

    // 逐字节喂进去。
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    let mut frames = Vec::new();
    for byte in &wire {
        frames.extend(decoder.push(&[*byte]).expect("push"));
    }
    decoder.end().expect("end");
    assert_eq!(frames, vec![vec![1, 2, 3], Vec::new(), vec![4]]);

    // 一次性喂进去。
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    assert_eq!(decoder.push(&wire).expect("push"), frames);
    decoder.end().expect("end");
}

#[test]
fn assembles_payloads_larger_than_one_chunk() {
    let payload: Vec<u8> = (0..70_000).map(|index| (index % 251) as u8).collect();
    let wire = encode_frame(&payload).expect("encode");
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    let mut frames = Vec::new();
    frames.extend(decoder.push(&wire[..101]).expect("push"));
    frames.extend(decoder.push(&wire[101..65_541]).expect("push"));
    frames.extend(decoder.push(&wire[65_541..]).expect("push"));
    decoder.end().expect("end");
    assert_eq!(frames, vec![payload]);
}

#[test]
fn handles_every_split_point() {
    let wire = encode_frame(&[10, 20, 30, 40]).expect("encode");
    for split in 0..=wire.len() {
        let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
        let mut frames = decoder.push(&wire[..split]).expect("push");
        frames.extend(decoder.push(&wire[split..]).expect("push"));
        decoder.end().expect("end");
        assert_eq!(frames, vec![vec![10, 20, 30, 40]], "split at {split}");
    }
}

#[test]
fn rejects_frames_over_the_configured_limit() {
    let wire = encode_frame(&[0; 8]).expect("encode");
    let mut decoder = FrameDecoder::new(FrameDecoderOptions {
        max_frame_length: Some(4),
    })
        .expect("decoder");
    let error = decoder.push(&wire).expect_err("frame too large");
    assert!(
        error.message().contains("exceeds configured limit"),
        "got: {error}"
    );
    // 失败后继续使用会报错。
    let error = decoder.push(&wire).expect_err("decoder has failed");
    assert!(error.message().contains("has failed"), "got: {error}");
}

#[test]
fn rejects_truncated_streams() {
    let wire = encode_frame(&[1, 2, 3]).expect("encode");
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    assert!(decoder.push(&wire[..5]).expect("push").is_empty());
    let error = decoder.end().expect_err("truncated");
    assert!(error.message().contains("Truncated frame"), "got: {error}");

    // 只有半个帧头也算截断。
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    decoder.push(&[0, 0]).expect("push");
    assert!(decoder.end().is_err());
}

#[test]
fn rejects_use_after_end() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    decoder.end().expect("end");
    let error = decoder.push(&[]).expect_err("ended");
    assert!(error.message().contains("has ended"), "got: {error}");
    assert!(decoder.end().is_err());
}
