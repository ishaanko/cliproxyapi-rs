//! Chunk-boundary independent response-model observation for streams (Go:
//! helps/stream_response_model_observer.go).

use super::text::trim_space;
use super::usage::UsageReporter;

const DEFAULT_MAX_STREAM_MODEL_BUFFER_BOUND: usize = 64 * 1024;
const DEFAULT_MAX_LINES_PER_STREAM_EVENT: usize = 2048;
const STREAM_EVENT_LINE_OVERHEAD: usize = 32;

/// Accumulates arbitrary network chunks in a bounded buffer, splits them into SSE lines and
/// events, and reports the served model to a [`UsageReporter`].
pub struct StreamResponseModelObserver {
    reporter: UsageReporter,
    buf: Vec<u8>,
    frame: Vec<Vec<u8>>,
    frame_bytes: usize,
    max_bound: usize,
    overflow: bool,
    event_overflow: bool,
}

impl StreamResponseModelObserver {
    pub fn new(reporter: UsageReporter) -> Self {
        Self {
            reporter,
            buf: Vec::new(),
            frame: Vec::new(),
            frame_bytes: 0,
            max_bound: DEFAULT_MAX_STREAM_MODEL_BUFFER_BOUND,
            overflow: false,
            event_overflow: false,
        }
    }

    fn drop_frame(&mut self) {
        self.frame.clear();
        self.frame_bytes = 0;
    }

    /// Ingests a raw chunk, splitting into lines and events.
    pub fn feed(&mut self, mut chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.reporter.is_response_model_final() {
            self.buf = Vec::new();
            self.frame.clear();
            return;
        }
        while !chunk.is_empty() {
            let idx = chunk.iter().position(|b| *b == b'\n');
            if self.overflow {
                let Some(idx) = idx else { return };
                chunk = &chunk[idx + 1..];
                self.overflow = false;
                self.buf.clear();
                continue;
            }
            let Some(idx) = idx else {
                if self.buf.len() + chunk.len() > self.max_bound {
                    self.overflow = true;
                    self.buf.clear();
                    if !self.frame.is_empty() {
                        self.event_overflow = true;
                        self.drop_frame();
                    }
                } else {
                    self.buf.extend_from_slice(chunk);
                }
                return;
            };
            let line_part = &chunk[..idx];
            chunk = &chunk[idx + 1..];
            if self.buf.len() + line_part.len() > self.max_bound {
                self.buf.clear();
                if !self.frame.is_empty() {
                    self.event_overflow = true;
                    self.drop_frame();
                }
                continue;
            }
            let line: Vec<u8> = if self.buf.is_empty() {
                strip_cr(line_part).to_vec()
            } else {
                self.buf.extend_from_slice(line_part);
                let line = strip_cr(&self.buf).to_vec();
                self.buf.clear();
                line
            };
            self.handle_line(&line);
            if self.reporter.is_response_model_final() {
                self.buf = Vec::new();
                self.frame.clear();
                return;
            }
        }
    }

    /// Flushes any pending buffered line or event at the end of the stream.
    pub fn finish(&mut self) {
        if !self.overflow && !self.buf.is_empty() {
            let line = strip_cr(&self.buf).to_vec();
            self.handle_line(&line);
            self.buf = Vec::new();
        }
        self.flush_event();
    }

    fn handle_line(&mut self, line: &[u8]) {
        let trimmed = trim_space(line);
        if trimmed.is_empty() {
            self.flush_event();
            return;
        }
        if self.event_overflow {
            return;
        }
        // Always try the line directly (`data: {"model":...}` or bare JSON).
        self.reporter.observe_response_model(line);
        if let Some(rest) = trimmed.strip_prefix(b"data:") {
            let data = rest.strip_prefix(b" ").unwrap_or(rest);
            let cost = data.len() + STREAM_EVENT_LINE_OVERHEAD;
            if self.frame_bytes + cost > self.max_bound || self.frame.len() >= DEFAULT_MAX_LINES_PER_STREAM_EVENT {
                self.event_overflow = true;
                self.drop_frame();
                return;
            }
            self.frame.push(data.to_vec());
            self.frame_bytes += cost;
        }
    }

    fn flush_event(&mut self) {
        if self.event_overflow {
            self.event_overflow = false;
            self.drop_frame();
            return;
        }
        if self.frame.is_empty() {
            return;
        }
        // A single-line event was already observed line by line.
        if self.frame.len() > 1 {
            let joined = self.frame.join(&b'\n');
            self.reporter.observe_response_model(&joined);
        }
        self.drop_frame();
    }
}

fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_found_across_chunk_boundaries_and_multiline_data() {
        let reporter = UsageReporter::new("claude", "ClaudeExecutor", "m", None, None);
        let mut obs = StreamResponseModelObserver::new(reporter.clone());
        obs.feed(b"event: message_start\ndata: {\"type\":\"message_st");
        obs.feed(b"art\",\"message\":{\"model\":\"claude-served\"}}\r\n\r\n");
        obs.finish();
        assert_eq!(reporter.response_model(), "claude-served");

        // A JSON object split over several data lines is joined at the event boundary.
        let reporter = UsageReporter::new("kimi", "KimiExecutor", "m", None, None);
        let mut obs = StreamResponseModelObserver::new(reporter.clone());
        obs.feed(b"data: {\"model\":\ndata: \"split\"}\n\n");
        assert_eq!(reporter.response_model(), "split");
    }

    #[test]
    fn oversized_lines_are_skipped_without_poisoning_later_events() {
        let reporter = UsageReporter::new("kimi", "KimiExecutor", "m", None, None);
        let mut obs = StreamResponseModelObserver::new(reporter.clone());
        obs.feed(&vec![b'x'; DEFAULT_MAX_STREAM_MODEL_BUFFER_BOUND + 10]);
        obs.feed(b"\ndata: {\"model\":\"after\"}\n\n");
        assert_eq!(reporter.response_model(), "after");
    }
}
