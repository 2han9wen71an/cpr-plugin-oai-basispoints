/// 增量 SSE 帧切分器：上游原始字节流按空行切成完整帧。
///
/// 帧字节保留原始 wire 形态（含 `event:`/`data:` 行与分隔空行），
/// 与宿主 provider 的 raw SSE frame 合同一致。
#[derive(Default)]
pub struct SseFrameSplitter {
    buffer: Vec<u8>,
}

/// 每个完整帧附带原始字节；`done` 标记 `data: [DONE]` 终止帧。
pub struct SseFrame {
    pub bytes: Vec<u8>,
    pub done: bool,
}

impl SseFrameSplitter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段上游字节，返回所有已完成的帧。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            let Some((split, separator)) = find_separator(&self.buffer) else {
                break;
            };
            let frame: Vec<u8> = self.buffer.drain(..split + separator).collect();
            let done = frame_is_done(&frame);
            frames.push(SseFrame { bytes: frame, done });
        }
        frames
    }

    /// 流结束时的残余字节；不完整帧按原样交付。
    pub fn finish(&mut self) -> Option<SseFrame> {
        if self.buffer.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.buffer);
        Some(SseFrame {
            done: frame_is_done(&bytes),
            bytes,
        })
    }
}

fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut lf = None;
    for window in buffer.windows(2).enumerate() {
        if window.1 == b"\n\n" {
            lf = Some((window.0, 2));
            break;
        }
    }
    let mut crlf = None;
    for window in buffer.windows(4).enumerate() {
        if window.1 == b"\r\n\r\n" {
            crlf = Some((window.0, 4));
            break;
        }
    }
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

/// 仅当帧的全部 data 行都是 `[DONE]` 时视为终止帧。
fn frame_is_done(frame: &[u8]) -> bool {
    let text = String::from_utf8_lossy(frame);
    let mut saw_data = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            saw_data = true;
            if data.trim() != "[DONE]" {
                return false;
            }
        }
    }
    saw_data
}
