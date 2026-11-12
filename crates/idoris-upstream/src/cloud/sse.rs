//! 增量 SSE（Server-Sent Events）解析：字节块 -> 完整的事件。
//!
//! 为什么自己写：要按**事件**转发，同时要在不缓冲整条流的前提下看见每个事件的内容（供应商请求号、流内 usage、
//! 错误事件）。规则按 WHATWG SSE：行以 `\n`、`\r\n` 或 `\r` 结束；空行结束一个事件；以 `:` 开头的行是注释；
//! `data:` 行（值去掉一个前导空格）用 `\n` 连接；流开头的 BOM 忽略。
//!
//! **转发用规范形式**：[`SseEvent::to_wire`] 把事件重新序列化成「每行以 `\n` 结束、事件以空行结束」，
//! 不管上游用的是 `\n`、`\r\n` 还是 `\r`。这样客户端看到的永远是同一种行结束符，也不用处理跨块的 `\r` / `\n`
//! 拆分。内容（每一行的字节）原样保留。
//!
//! 与既有 `idoris-router::sse::ensure_terminated` 的立场一致：**文件结束时没有以空行结束的事件不算数**
//! （[`SseParser::has_partial_event`] 为真时调用方应当把这条流当作被截断，而不是当作完成）。
//!
//! 事件大小有上限（`max_event_bytes`），超出返回 [`SseError::EventTooLarge`]：一个不带换行的无限长「行」
//! 不能把内存吃光。

/// 一个完整的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// 事件里的每一行（不含行结束符，不含空行）。
    lines: Vec<Vec<u8>>,
    /// `data:` 字段的内容（多行用 `\n` 连接）；没有 `data` 字段时为 `None`。可能不是合法的 UTF-8。
    pub data: Option<Vec<u8>>,
    /// 事件里只有注释行（`: keep-alive` 一类）。
    pub comment_only: bool,
}

impl SseEvent {
    /// 规范形式的字节：每行以 `\n` 结束，事件以一个空行结束。
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.lines.iter().map(|l| l.len() + 1).sum::<usize>() + 1);
        for line in &self.lines {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        out.push(b'\n');
        out
    }

    /// 测试和构造用：从若干行直接造一个事件。
    pub fn from_lines(lines: &[&[u8]]) -> Self {
        let mut parser = SseParser::new(usize::MAX);
        let mut wire = Vec::new();
        for line in lines {
            wire.extend_from_slice(line);
            wire.push(b'\n');
        }
        wire.push(b'\n');
        let mut events = parser.push(&wire).unwrap_or_default();
        events.pop().unwrap_or(SseEvent {
            lines: Vec::new(),
            data: None,
            comment_only: false,
        })
    }
}

/// SSE 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("an SSE event exceeds the configured size limit")]
    EventTooLarge,
}

/// 增量解析器。`push` 喂入字节块，返回这一块里补全的所有事件。
#[derive(Debug)]
pub struct SseParser {
    max_event_bytes: usize,
    /// 当前事件里已经完成的非空行。
    lines: Vec<Vec<u8>>,
    /// 当前事件已累计的字节数（只用于上限）。
    event_bytes: usize,
    /// 当前行（不含行结束符）。
    line: Vec<u8>,
    data_lines: Vec<Vec<u8>>,
    saw_field: bool,
    saw_comment: bool,
    /// 上一个字节是 `\r`：如果下一个字节是 `\n`，它属于同一个行结束符，直接跳过。
    after_cr: bool,
    /// 流开头的 BOM 检测。
    bom: BomState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BomState {
    /// 已经收到的 BOM 前缀字节数（0..=2），还在判断。
    Checking(u8),
    Done,
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl SseParser {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            lines: Vec::new(),
            event_bytes: 0,
            line: Vec::new(),
            data_lines: Vec::new(),
            saw_field: false,
            saw_comment: false,
            after_cr: false,
            bom: BomState::Checking(0),
        }
    }

    /// 有一个已经开始、但还没有以空行结束的事件（或一行写到一半）。流结束时若为真，说明被截断了。
    pub fn has_partial_event(&self) -> bool {
        !self.lines.is_empty()
            || !self.line.is_empty()
            || matches!(self.bom, BomState::Checking(n) if n > 0)
    }

    /// 喂入一块字节，返回补全的事件。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut events = Vec::new();
        for &byte in chunk {
            // BOM：只在流的最开头
            if let BomState::Checking(seen) = self.bom {
                if byte == BOM[usize::from(seen)] {
                    self.bom = if usize::from(seen) + 1 == BOM.len() {
                        BomState::Done
                    } else {
                        BomState::Checking(seen + 1)
                    };
                    continue;
                }
                // 不是 BOM：把已经「吞」下的前缀字节还给内容
                let prefix: Vec<u8> = BOM[..usize::from(seen)].to_vec();
                self.bom = BomState::Done;
                for b in prefix {
                    self.feed(b, &mut events)?;
                }
            }
            self.feed(byte, &mut events)?;
        }
        Ok(events)
    }

    fn feed(&mut self, byte: u8, events: &mut Vec<SseEvent>) -> Result<(), SseError> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                // `\r\n`：这个 `\n` 属于上一个行结束符
                return Ok(());
            }
        }
        match byte {
            b'\r' | b'\n' => {
                self.after_cr = byte == b'\r';
                self.end_line(events);
            }
            other => {
                self.line.push(other);
                if self.event_bytes + self.line.len() > self.max_event_bytes {
                    return Err(SseError::EventTooLarge);
                }
            }
        }
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) {
        if self.line.is_empty() {
            // 空行：事件边界。事件开头多余的空行直接丢弃。
            if self.lines.is_empty() {
                return;
            }
            let data = if self.data_lines.is_empty() {
                None
            } else {
                Some(self.data_lines.join(&b'\n'))
            };
            events.push(SseEvent {
                lines: std::mem::take(&mut self.lines),
                data,
                comment_only: self.saw_comment && !self.saw_field && self.data_lines.is_empty(),
            });
            self.data_lines.clear();
            self.event_bytes = 0;
            self.saw_field = false;
            self.saw_comment = false;
            return;
        }
        let line = std::mem::take(&mut self.line);
        self.event_bytes += line.len() + 1;
        if line.first() == Some(&b':') {
            self.saw_comment = true;
        } else {
            let (name, value) = match line.iter().position(|&b| b == b':') {
                Some(pos) => {
                    let value = &line[pos + 1..];
                    let value = value.strip_prefix(b" ").unwrap_or(value);
                    (&line[..pos], value)
                }
                None => (&line[..], &line[line.len()..]),
            };
            if name == b"data" {
                self.data_lines.push(value.to_vec());
            } else {
                self.saw_field = true;
            }
        }
        self.lines.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> (Vec<SseEvent>, bool) {
        let mut parser = SseParser::new(1 << 20);
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.push(chunk).unwrap());
        }
        (events, parser.has_partial_event())
    }

    fn data_strings(events: &[SseEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| e.data.as_ref())
            .map(|d| String::from_utf8(d.clone()).unwrap())
            .collect()
    }

    #[test]
    fn splits_events_and_re_serializes_them_canonically() {
        let wire = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let (events, partial) = parse_all(&[wire]);
        assert!(!partial);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].to_wire(), b"data: {\"a\":1}\n\n");
        assert_eq!(events[1].to_wire(), b"data: [DONE]\n\n");
        assert_eq!(data_strings(&events), vec!["{\"a\":1}", "[DONE]"]);
        // 规范形式拼起来就是原流（原流本来就是规范形式）
        let joined: Vec<u8> = events.iter().flat_map(SseEvent::to_wire).collect();
        assert_eq!(joined, wire);
    }

    #[test]
    fn crlf_and_cr_streams_are_forwarded_with_plain_newlines() {
        let (events, _) = parse_all(&[b"data: a\r\n\r\ndata: b\r\r"]);
        let joined: Vec<u8> = events.iter().flat_map(SseEvent::to_wire).collect();
        assert_eq!(joined, b"data: a\n\ndata: b\n\n");
    }

    #[test]
    fn every_possible_chunk_split_gives_the_same_events() {
        let wire = b": hello\r\n\r\ndata: one\r\ndata: two\r\n\r\ndata:three\n\n\r\ndata: x\r\r";
        let (whole, whole_partial) = parse_all(&[wire]);
        for cut in 0..=wire.len() {
            let (left, right) = wire.split_at(cut);
            let (events, partial) = parse_all(&[left, right]);
            assert_eq!(events, whole, "cut at {cut}");
            assert_eq!(partial, whole_partial, "cut at {cut}");
        }
        // 逐字节喂
        let singles: Vec<&[u8]> = wire.chunks(1).collect();
        let (events, partial) = parse_all(&singles);
        assert_eq!(events, whole);
        assert_eq!(partial, whole_partial);
        assert_eq!(
            data_strings(&whole),
            vec!["one\ntwo", "three", "x"],
            "multi-line data is joined with a newline; `data:three` (no space) is accepted"
        );
        assert!(!whole_partial);
    }

    #[test]
    fn all_three_line_endings_are_event_boundaries() {
        for terminator in ["\n", "\r\n", "\r"] {
            let wire = format!("data: a{terminator}{terminator}data: b{terminator}{terminator}");
            let (events, partial) = parse_all(&[wire.as_bytes()]);
            assert!(!partial, "{terminator:?}");
            assert_eq!(data_strings(&events), vec!["a", "b"], "{terminator:?}");
        }
    }

    #[test]
    fn comments_are_flagged_and_do_not_carry_data() {
        let (events, _) = parse_all(&[b": OPENROUTER PROCESSING\n\ndata: x\n\n"]);
        assert_eq!(events.len(), 2);
        assert!(events[0].comment_only);
        assert!(events[0].data.is_none());
        assert_eq!(events[0].to_wire(), b": OPENROUTER PROCESSING\n\n");
        assert!(!events[1].comment_only);
        // 注释和数据在同一个事件里：不是 comment_only
        let (events, _) = parse_all(&[b": c\ndata: y\n\n"]);
        assert!(!events[0].comment_only);
        assert_eq!(data_strings(&events), vec!["y"]);
        // 只有 event 字段没有 data：不是注释，但没有数据
        let (events, _) = parse_all(&[b"event: ping\n\n"]);
        assert_eq!(events.len(), 1);
        assert!(!events[0].comment_only);
        assert!(events[0].data.is_none());
    }

    #[test]
    fn an_event_without_a_closing_blank_line_is_partial_not_complete() {
        let (events, partial) = parse_all(&[b"data: {\"a\":1}\n\ndata: {\"b\":2}\n"]);
        assert_eq!(events.len(), 1, "the second event never finished");
        assert!(partial);
        let (events, partial) = parse_all(&[b"data: half a lin"]);
        assert!(events.is_empty());
        assert!(partial);
        let (events, partial) = parse_all(&[b"data: x\n\n"]);
        assert_eq!(events.len(), 1);
        assert!(!partial);
        let (events, partial) = parse_all(&[b""]);
        assert!(events.is_empty());
        assert!(!partial);
        // 事件之间多余的空行不产生空事件
        let (events, partial) = parse_all(&[b"\n\n\ndata: x\n\n\n\n"]);
        assert_eq!(events.len(), 1);
        assert!(!partial);
    }

    #[test]
    fn a_bom_at_the_start_is_ignored_but_not_in_the_middle() {
        let mut wire = vec![0xEF, 0xBB, 0xBF];
        wire.extend_from_slice(b"data: x\n\n");
        let (events, _) = parse_all(&[&wire]);
        assert_eq!(data_strings(&events), vec!["x"]);
        // 拆开喂 BOM
        let (events, _) = parse_all(&[&wire[..1], &wire[1..2], &wire[2..]]);
        assert_eq!(data_strings(&events), vec!["x"]);
        // 以 0xEF 开头但不是 BOM：字节归还给内容，不丢
        let (events, _) = parse_all(&[b"\xEFdata: x\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].to_wire(), b"\xEFdata: x\n\n");
    }

    #[test]
    fn invalid_utf8_in_data_is_carried_as_bytes() {
        let (events, _) = parse_all(&[b"data: \xFF\xFE\n\n"]);
        assert_eq!(events[0].data.as_deref(), Some(&[0xFF, 0xFE][..]));
    }

    #[test]
    fn an_oversized_event_is_an_error_not_unbounded_memory() {
        let mut parser = SseParser::new(64);
        let long_line = vec![b'a'; 100];
        let err = parser.push(&long_line).unwrap_err();
        assert_eq!(err, SseError::EventTooLarge);
        // 很多短行累计超过上限也一样
        let mut parser = SseParser::new(64);
        let mut wire = Vec::new();
        for _ in 0..40 {
            wire.extend_from_slice(b"data: x\n");
        }
        assert_eq!(parser.push(&wire).unwrap_err(), SseError::EventTooLarge);
        // 恰好在上限内：可以
        let mut parser = SseParser::new(64);
        let mut ok = b"data: ".to_vec();
        ok.extend(std::iter::repeat_n(b'a', 50));
        ok.extend_from_slice(b"\n\n");
        assert_eq!(parser.push(&ok).unwrap().len(), 1);
    }

    #[test]
    fn from_lines_builds_the_same_event_the_parser_would() {
        let built = SseEvent::from_lines(&[b"data: {\"id\":\"x\"}"]);
        let (parsed, _) = parse_all(&[b"data: {\"id\":\"x\"}\n\n"]);
        assert_eq!(vec![built], parsed);
    }
}
