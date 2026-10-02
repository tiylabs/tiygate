//! Bounded byte framing for SSE. UTF-8 is decoded only after a complete line.

const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SseFramer {
    line: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    after_cr: bool,
    first_line: bool,
}

impl SseFramer {
    pub(super) fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(&mut events)?;
                self.after_cr = byte == b'\r';
            } else {
                if self.line.len() + self.data_bytes >= MAX_EVENT_BYTES {
                    return Err("upstream SSE event exceeds 16 MiB".into());
                }
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    pub(super) fn finish(&mut self) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.finish_line(&mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn finish_line(&mut self, events: &mut Vec<String>) -> Result<(), String> {
        let bytes = std::mem::take(&mut self.line);
        let mut line = std::str::from_utf8(&bytes)
            .map_err(|_| "upstream SSE contains invalid UTF-8".to_string())?;
        if self.first_line {
            self.first_line = false;
            line = line.trim_start_matches('\u{feff}');
        }
        if line.is_empty() {
            self.dispatch(events);
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.data_bytes += value.len() + 1;
            self.data.push(value.to_string());
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(format!("data: {}", self.data.join("\n")));
            self.data.clear();
            self.data_bytes = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseFramer;

    #[test]
    fn every_byte_boundary_preserves_utf8_crlf_and_multiline_data() -> Result<(), String> {
        let bytes =
            "\u{feff}: ping\r\ndata:{\"text\":\"你好😀\",\r\ndata: \"n\":1}\r\n\r\n".as_bytes();
        for cut in 0..=bytes.len() {
            let mut framer = SseFramer::new();
            let mut events = framer.feed(&bytes[..cut])?;
            events.extend(framer.feed(&bytes[cut..])?);
            events.extend(framer.finish()?);
            assert_eq!(events, vec!["data: {\"text\":\"你好😀\",\n\"n\":1}"]);
        }
        Ok(())
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        assert!(SseFramer::new().feed(b"data: \xff\n\n").is_err());
    }
}
