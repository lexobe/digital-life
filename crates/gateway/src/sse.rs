//! Incremental SSE framing: LF, CRLF, and CR; split UTF-8 and multiline data.

#[derive(Default)]
pub(crate) struct Decoder {
    line: Vec<u8>,
    lines: Vec<String>,
    size: usize,
    skip_lf: bool,
    started: bool,
}

#[derive(Debug)]
pub(crate) struct Frame {
    pub raw: String,
    pub data: Option<String>,
    pub event: Option<String>,
}

impl Decoder {
    pub fn push(&mut self, chunk: &[u8], limit: usize) -> Result<Vec<Frame>, ()> {
        let mut frames = Vec::new();
        for &byte in chunk {
            if self.skip_lf && byte == b'\n' {
                self.skip_lf = false;
                continue;
            }
            self.skip_lf = byte == b'\r';
            self.size = self.size.checked_add(1).ok_or(())?;
            if self.size > limit {
                return Err(());
            }
            if byte == b'\n' || byte == b'\r' {
                let bytes = std::mem::take(&mut self.line);
                let mut line = String::from_utf8(bytes).map_err(|_| ())?;
                if !self.started {
                    self.started = true;
                    line = line.trim_start_matches('\u{feff}').to_owned();
                }
                if line.is_empty() {
                    if !self.lines.is_empty() {
                        frames.push(Frame::new(std::mem::take(&mut self.lines)));
                    }
                    self.size = 0;
                } else {
                    self.lines.push(line);
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(frames)
    }

    pub fn is_empty(&self) -> bool {
        self.line.is_empty() && self.lines.is_empty()
    }
}

impl Frame {
    fn new(lines: Vec<String>) -> Self {
        let mut data = Vec::new();
        let mut event = None;
        for line in &lines {
            if line.starts_with(':') {
                continue;
            }
            let (key, value) = line.split_once(':').unwrap_or((line.as_str(), ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match key {
                "data" => data.push(value),
                "event" => event = Some(value.to_owned()),
                _ => {}
            }
        }
        Self {
            raw: format!("{}\n\n", lines.join("\n")),
            data: (!data.is_empty()).then(|| data.join("\n")),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_utf8_bom_crlf_and_multiline() {
        let input =
            "\u{feff}: ping\r\n\r\nevent: response.created\rdata: {\rdata: \"text\":\"雪\"}\r\r";
        let mut decoder = Decoder::default();
        let mut frames = vec![];
        for byte in input.as_bytes() {
            frames.extend(decoder.push(&[*byte], 1024).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert!(frames[0].data.is_none());
        assert_eq!(frames[1].event.as_deref(), Some("response.created"));
        assert_eq!(frames[1].data.as_deref(), Some("{\n\"text\":\"雪\"}"));
        assert!(decoder.is_empty());
    }

    #[test]
    fn rejects_oversized_and_invalid_utf8_frames() {
        assert!(Decoder::default().push(b"data: too long", 4).is_err());
        assert!(Decoder::default().push(&[255, b'\n'], 10).is_err());
        let mut decoder = Decoder::default();
        decoder.push(b"data: incomplete", 100).unwrap();
        assert!(!decoder.is_empty());
    }
}
