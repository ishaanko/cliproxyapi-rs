//! Force-mapped model name rewriting for streams (Go: response_model_rewriter.go).
//!
//! When an alias is configured with `force-mapping`, the client must see the alias in the
//! response `model` field even though the upstream reports its own name. Non-stream responses are
//! rewritten by `models::rewrite_model_in_response`; streams go through [`StreamRewriter`], which
//! understands SSE framing, raw JSON chunks and frames split across chunk boundaries.

use cpa_json::J;

const MODEL_FIELD_PATHS: [&str; 5] = ["model", "modelVersion", "response.model", "response.modelVersion", "message.model"];
const MAX_PENDING_BUF_SIZE: usize = 1 << 20;

fn valid_json(data: &[u8]) -> bool {
    cpa_json::valid(data)
}

/// Rewrites known model fields in one JSON document.
fn rewrite_model(data: &[u8], target: &str) -> Vec<u8> {
    if target.is_empty() || data.is_empty() {
        return data.to_vec();
    }
    let mut v = cpa_json::parse(data);
    if v.is_null() {
        return data.to_vec();
    }
    let mut changed = false;
    for path in MODEL_FIELD_PATHS {
        if v.g(path).exists() {
            cpa_json::set(&mut v, path, target);
            changed = true;
        }
    }
    if changed { cpa_json::to_vec(&v) } else { data.to_vec() }
}

fn extract_sse_data_line(line: &[u8]) -> Option<(&'static [u8], &[u8])> {
    if let Some(rest) = line.strip_prefix(b"data: ") {
        return Some((b"data: ", rest));
    }
    line.strip_prefix(b"data:").map(|rest| (&b"data:"[..], rest))
}

fn split_lines(payload: &[u8]) -> Vec<&[u8]> {
    payload.split(|b| *b == b'\n').collect()
}

fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.join(&b'\n')
}

/// Rewrites every `data: {json}` line of a payload (line-wise fallback).
pub fn rewrite_sse_payload_lines(payload: &[u8], target: &str) -> Vec<u8> {
    if target.is_empty() || payload.is_empty() {
        return payload.to_vec();
    }
    let out: Vec<Vec<u8>> = split_lines(payload)
        .into_iter()
        .map(|line| match extract_sse_data_line(line) {
            Some((prefix, json)) if !json.is_empty() && json[0] == b'{' && valid_json(json) => {
                let mut l = prefix.to_vec();
                l.extend(rewrite_model(json, target));
                l
            }
            _ => line.to_vec(),
        })
        .collect();
    let mut joined = join_lines(&out);
    if payload.last() == Some(&b'\n') && joined.last() != Some(&b'\n') {
        joined.push(b'\n');
    }
    joined
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn safe_replace_glued(chunk: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if find(chunk, old).is_none() {
        return chunk.to_vec();
    }
    let mut result = Vec::with_capacity(chunk.len() + 8);
    let mut remaining = chunk;
    loop {
        let Some(idx) = find(remaining, old) else {
            result.extend_from_slice(remaining);
            break;
        };
        let line_start = remaining[..idx].iter().rposition(|b| *b == b'\n');
        let part = match line_start {
            None => &remaining[..idx + 1],
            Some(ls) => &remaining[ls + 1..idx + 1],
        };
        let closes_valid_data = extract_sse_data_line(part).is_some_and(|(_, json)| !json.is_empty() && valid_json(json));
        if closes_valid_data {
            result.extend_from_slice(&remaining[..idx]);
            result.extend_from_slice(new);
            remaining = &remaining[idx + old.len()..];
            continue;
        }
        result.extend_from_slice(&remaining[..idx + old.len()]);
        remaining = &remaining[idx + old.len()..];
    }
    result
}

fn normalize_glued_sse_events(chunk: &[u8]) -> Vec<u8> {
    if chunk.is_empty() {
        return Vec::new();
    }
    // Antigravity/Gemini frames glue as "...}event:..."; Codex data lines as "...}data:...".
    let c = safe_replace_glued(chunk, b"}event:", b"}\n\nevent:");
    let c = safe_replace_glued(&c, b"}\r\nevent:", b"}\r\n\r\nevent:");
    let c = safe_replace_glued(&c, b"}data:", b"}\ndata:");
    safe_replace_glued(&c, b"}\r\ndata:", b"}\r\ndata:")
}

fn extract_last_data_payload(chunk: &[u8]) -> Option<&[u8]> {
    split_lines(chunk)
        .into_iter()
        .rev()
        .find_map(|l| extract_sse_data_line(l).map(|(_, j)| j).filter(|j| !j.is_empty()))
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |e| e + 1);
    &b[start..end]
}

/// Rewrites model names in streaming responses; buffers a trailing partial frame.
pub struct StreamRewriter {
    model: String,
    pending: Vec<u8>,
}

impl StreamRewriter {
    pub fn new(rewrite_model: impl Into<String>) -> Self {
        StreamRewriter { model: rewrite_model.into(), pending: Vec::new() }
    }

    fn rewrite_sse_lines(&self, payload: &[u8]) -> Vec<u8> {
        rewrite_sse_payload_lines(payload, &self.model)
    }

    /// Rewrites one chunk. `None` means the chunk was buffered as an incomplete frame.
    pub fn rewrite_chunk(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if self.model.is_empty() {
            return Some(chunk.to_vec());
        }
        let mut chunk = chunk.to_vec();
        if !self.pending.is_empty() {
            let mut combined = std::mem::take(&mut self.pending);
            if combined.last() != Some(&b'\n') {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&chunk);
            chunk = combined;
        }
        let chunk = normalize_glued_sse_events(&chunk);
        if chunk.len() > MAX_PENDING_BUF_SIZE {
            return Some(chunk);
        }

        // Raw JSON chunk (Gemini/OpenAI without the SSE "data:" prefix).
        let trimmed = trim_ascii(&chunk);
        if !trimmed.is_empty() && trimmed[0] == b'{' && valid_json(trimmed) {
            return Some(rewrite_model(trimmed, &self.model));
        }

        let process: Vec<u8>;
        if let Some(last_double) = rfind(&chunk, b"\n\n") {
            let after = &chunk[last_double + 2..];
            if !after.is_empty() && after != b"\n" {
                process = chunk[..last_double + 2].to_vec();
                self.pending = after.to_vec();
            } else {
                process = chunk.clone();
            }
        } else if extract_last_data_payload(&chunk).is_some_and(valid_json) {
            process = chunk.clone();
        } else if trim_ascii(&chunk).is_empty() {
            return Some(chunk);
        } else if !chunk.is_empty() {
            self.pending = chunk;
            return None;
        } else {
            return Some(chunk);
        }

        let lines = split_lines(&process);
        let mut result: Vec<Vec<u8>> = Vec::new();
        let mut pending_event: Option<Vec<u8>> = None;
        for line in lines {
            if line.starts_with(b"event:") {
                pending_event = Some(line.to_vec());
                continue;
            }
            if let Some((prefix, json)) = extract_sse_data_line(line)
                && !json.is_empty()
                && json[0] == b'{'
            {
                if !valid_json(json) {
                    match pending_event.take() {
                        Some(ev) => {
                            self.pending = ev;
                            self.pending.push(b'\n');
                            self.pending.extend_from_slice(line);
                        }
                        None => self.pending.extend_from_slice(line),
                    }
                    continue;
                }
                if let Some(ev) = pending_event.take() {
                    result.push(ev);
                }
                let mut l = prefix.to_vec();
                l.extend(rewrite_model(json, &self.model));
                result.push(l);
                continue;
            }
            if let Some(ev) = pending_event.take() {
                result.push(ev);
            }
            result.push(line.to_vec());
        }
        if let Some(ev) = pending_event.take() {
            result.push(ev);
        }
        let joined = join_lines(&result);
        if joined.is_empty() && !chunk.is_empty() {
            return Some(self.rewrite_sse_lines(&chunk));
        }
        Some(joined)
    }

    /// Flushes buffered partial data at the end of the stream.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(b"\n\n");
        let buf = normalize_glued_sse_events(&buf);
        let mut out = self.rewrite_chunk(&buf).unwrap_or_default();
        if !self.pending.is_empty() {
            let leftover = std::mem::take(&mut self.pending);
            let tail = self.rewrite_sse_lines(&leftover);
            out.extend(tail);
        }
        if out.is_empty() { None } else { Some(out) }
    }

    /// Chunk-level entry used by the stream wrapper (Go: rewriteForceMappedStreamChunk).
    pub fn rewrite_payload(&mut self, payload: &[u8]) -> Vec<u8> {
        if payload.is_empty() {
            return Vec::new();
        }
        if let Some(rewritten) = self.rewrite_chunk(payload)
            && !rewritten.is_empty()
        {
            return rewritten;
        }
        if find(payload, b"data:").is_some() {
            let line_wise = self.rewrite_sse_lines(payload);
            if !line_wise.is_empty() {
                return line_wise;
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut r = StreamRewriter::new("alias");
        let mut out = String::new();
        for c in chunks {
            out.push_str(&String::from_utf8(r.rewrite_payload(c.as_bytes())).unwrap());
        }
        if let Some(tail) = r.finish() {
            out.push_str(&String::from_utf8(tail).unwrap());
        }
        out
    }

    #[test]
    fn rewrites_sse_data_frames_and_keeps_events() {
        let out = run(&["event: message_start\ndata: {\"message\":{\"model\":\"up\"}}\n\n"]);
        assert_eq!(out, "event: message_start\ndata: {\"message\":{\"model\":\"alias\"}}\n\n");
    }

    #[test]
    fn rewrites_raw_json_and_glued_codex_lines() {
        assert_eq!(run(&[r#"{"model":"up","x":1}"#]), r#"{"model":"alias","x":1}"#);
        let out = run(&["data: {\"response\":{\"model\":\"up\"}}data: {\"model\":\"up\"}"]);
        assert!(out.contains("\"response\":{\"model\":\"alias\"}"), "{out}");
        assert_eq!(out.matches("alias").count(), 2, "{out}");
    }

    #[test]
    fn done_marker_and_non_json_pass_through() {
        let out = run(&["data: [DONE]\n\n"]);
        assert_eq!(out, "data: [DONE]\n\n");
    }

    #[test]
    fn data_prefix_without_space_is_preserved() {
        let mut r = StreamRewriter::new("k2.5");
        let out = r
            .rewrite_chunk(b"event:message_start\ndata:{\"type\":\"message_start\",\"message\":{\"model\":\"kimi-k2.5\"}}\n\n")
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("\"model\":\"k2.5\"") && out.contains("data:{") && !out.contains("kimi-k2.5"), "{out}");
    }

    #[test]
    fn event_line_buffers_until_its_data_frame_arrives_without_duplication() {
        let mut r = StreamRewriter::new("gpt-5.4-fast");
        assert_eq!(r.rewrite_chunk(b"event: response.created\n"), None);
        let out = r
            .rewrite_chunk(b"data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.4\"}}\n\n")
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.matches("event: response.created").count(), 1, "{out}");
        assert!(out.ends_with("\n\n") && out.contains("\"model\":\"gpt-5.4-fast\""), "{out}");
        assert!(r.finish().is_none());
    }

    #[test]
    fn codex_line_by_line_chunks_are_all_rewritten() {
        let mut r = StreamRewriter::new("gpt-5.4-fast");
        let lines: [&[u8]; 6] = [
            b"event: response.created\n",
            b"data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.4\"}}\n",
            b"\n",
            b"event: response.completed\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.4\"}}\n",
            b"\n",
        ];
        let mut out = Vec::new();
        for l in lines {
            if let Some(c) = r.rewrite_chunk(l) {
                out.extend(c);
            }
        }
        if let Some(t) = r.finish() {
            out.extend(t);
        }
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.matches("gpt-5.4-fast").count(), 2, "{out}");
        assert!(!out.contains("\"model\":\"gpt-5.4\""), "{out}");
    }
}
