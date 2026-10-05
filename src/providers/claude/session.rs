//! Claude Code session logs: the current session's model, tokens and context use.

use super::jsonl::FileInfo;
use crate::model::Session;
use serde::Deserialize;

const DEFAULT_CONTEXT: u64 = 200_000;
const LONG_CONTEXT: u64 = 1_000_000;
const SYNTHETIC_MODEL: &str = "<synthetic>";

pub fn context_limit(model: &str, limit_override: Option<u64>) -> u64 {
    limit_override.unwrap_or(if model.contains("[1m]") {
        LONG_CONTEXT
    } else {
        DEFAULT_CONTEXT
    })
}

#[derive(Deserialize)]
struct LogLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    effort: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LogUsage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

/// Aggregates a Claude Code session log; `None` when it has no assistant usage.
pub fn scan_session(text: &str, limit_override: Option<u64>) -> Option<Session> {
    let mut s = Session {
        model: String::new(),
        effort: None,
        input: 0,
        output: 0,
        cache_create: 0,
        cache_read: 0,
        requests: 0,
        context_tokens: 0,
        context_limit: 0,
    };
    let mut seen = std::collections::HashSet::new();
    for line in text.lines().filter(|l| l.contains("\"assistant\"")) {
        let Ok(parsed) = serde_json::from_str::<LogLine>(line) else {
            continue;
        };
        if parsed.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(msg) = parsed.message else { continue };
        let model = msg.model.unwrap_or_default();
        if model == SYNTHETIC_MODEL {
            continue;
        }
        let Some(u) = msg.usage else { continue };
        // One API response spans several lines (one per content block) with identical usage.
        if msg.id.is_some() && !seen.insert((msg.id, parsed.request_id)) {
            continue;
        }
        s.input += u.input_tokens;
        s.output += u.output_tokens;
        s.cache_create += u.cache_creation_input_tokens;
        s.cache_read += u.cache_read_input_tokens;
        s.requests += 1;
        s.context_tokens =
            u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
        if !model.is_empty() {
            s.model = model;
        }
        if parsed.effort.is_some() {
            s.effort = parsed.effort;
        }
    }
    if s.requests == 0 {
        return None;
    }
    // Model ids do not always carry "[1m]"; a context above the default proves the long window.
    let limit = context_limit(&s.model, limit_override);
    s.context_limit = if limit_override.is_none() && s.context_tokens > limit {
        LONG_CONTEXT
    } else {
        limit
    };
    Some(s)
}

pub(super) struct SessionCache {
    pub(super) file: FileInfo,
    pub(super) session: Option<Session>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude::test_support::SESSION;

    #[test]
    fn scan_session_sums_usage_and_tracks_last_context() {
        let s = scan_session(SESSION, None).unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.input, 180);
        assert_eq!(s.output, 1_800);
        assert_eq!(s.cache_create, 2_200);
        assert_eq!(s.cache_read, 101_920);
        assert_eq!(s.context_tokens, 80 + 200 + 71_920);
        assert_eq!(s.context_limit, 200_000);
        assert_eq!(s.model, "claude-sonnet-5");
        assert_eq!(s.effort.as_deref(), Some("high"));
    }

    #[test]
    fn scan_session_ignores_synthetic_entries() {
        let text = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "\n"
        );
        let s = scan_session(text, None).unwrap();
        assert_eq!(s.model, "claude-opus-5-5");
        assert_eq!(s.context_tokens, 8);
        assert_eq!(s.requests, 1);
    }

    #[test]
    fn scan_session_without_assistant_usage_is_none() {
        assert!(scan_session("not json\n{\"type\":\"user\"}\n", None).is_none());
    }

    #[test]
    fn scan_session_counts_each_api_response_once() {
        // Claude Code writes one line per content block, repeating id, requestId and usage.
        let block = r#"{"type":"assistant","requestId":"req_1","message":{"id":"msg_1","model":"claude-sonnet-5","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":30,"cache_read_input_tokens":40}}}"#;
        let other = r#"{"type":"assistant","requestId":"req_2","message":{"id":"msg_2","model":"claude-sonnet-5","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}}}"#;
        let text = [block, block, block, other].join("\n");
        let s = scan_session(&text, None).unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(
            (s.input, s.output, s.cache_create, s.cache_read),
            (11, 22, 33, 44)
        );
        assert_eq!(s.context_tokens, 1 + 3 + 4);
    }

    #[test]
    fn scan_session_infers_long_context_when_over_default() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":379000}}}"#;
        let s = scan_session(line, None).unwrap();
        assert_eq!(s.context_limit, 1_000_000);
        assert!(s.context_pct() < 100.0);
    }

    #[test]
    fn context_limit_rules() {
        assert_eq!(context_limit("claude-sonnet-5", None), 200_000);
        assert_eq!(context_limit("claude-sonnet-5[1m]", None), 1_000_000);
        assert_eq!(context_limit("claude-sonnet-5[1m]", Some(500_000)), 500_000);
    }
}
