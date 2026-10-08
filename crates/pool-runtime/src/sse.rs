//! Incremental Server-Sent Events parsing. Network reads are not event
//! boundaries: an event (and a UTF-8 character) may arrive split across chunks.

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SseEvent {
    /// The joined `data:` lines of one event.
    Data(String),
    /// `data: [DONE]`
    Done,
}

impl SseParser {
    /// Largest event accepted; a longer one means a broken upstream.
    pub const MAX_EVENT_BYTES: usize = 1 << 20;

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((end, separator)) = find_event_end(&self.buffer) {
            let raw: Vec<u8> = self.buffer.drain(..end + separator).collect();
            let text = std::str::from_utf8(&raw[..end]).map_err(|_| "upstream event is not UTF-8".to_string())?;
            let mut data = Vec::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest));
                }
            }
            if data.is_empty() {
                continue; // comments, keep-alives, other fields
            }
            let data = data.join("\n");
            events.push(if data.trim() == "[DONE]" { SseEvent::Done } else { SseEvent::Data(data) });
        }
        if self.buffer.len() > Self::MAX_EVENT_BYTES {
            return Err("upstream event exceeds 1 MiB".into());
        }
        Ok(events)
    }
}

/// Position of the blank line ending the first event, and its length.
fn find_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buffer.len() {
        if buffer[i] == b'\n' && buffer[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buffer[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_split_frames_and_utf8() {
        let stream = "data: {\"a\":\"é\"}\n\n: keep-alive\n\ndata: [DONE]\n\n".as_bytes();
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in stream {
            events.extend(parser.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(events, vec![SseEvent::Data("{\"a\":\"é\"}".into()), SseEvent::Done]);
    }

    #[test]
    fn handles_crlf() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: 1\r\n\r\ndata: 2\r\n\r\n").unwrap();
        assert_eq!(events, vec![SseEvent::Data("1".into()), SseEvent::Data("2".into())]);
    }
}
