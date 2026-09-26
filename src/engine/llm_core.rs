//! Engine-independent LLM rules: context budget, BOS handling, reasoning
//! split and the `chat.completion` response shape.  No llama.cpp here, so
//! every platform and every CI leg tests it.

use crate::types::{ChatMessage, LlmParams};

/// Context window when the catalogue does not set `contextSize`.  Covers
/// ordinary chat; long-prompt models (summarisers: ~10k-token prompts)
/// set their own.  Safe range 2048..=the model's training context.
pub const DEFAULT_CONTEXT_SIZE: u32 = 8192;

/// Why generation stopped, as OpenAI names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Stop,
    Length,
}

impl Finish {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
        }
    }
}

/// A prompt that leaves no room to answer.  Refused, never truncated:
/// cutting the operator's prompt would change what they asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "prompt is {prompt_tokens} tokens but the context holds {n_ctx}; raise contextSize or \
     shorten the prompt"
)]
pub struct ContextOverflow {
    pub prompt_tokens: usize,
    pub n_ctx: u32,
}

/// Tokens this request may generate: `max_tokens` (at least one), clamped
/// to the room the prompt leaves in the context.
pub fn plan_budget(
    prompt_tokens: usize,
    max_tokens: u32,
    n_ctx: u32,
) -> Result<u32, ContextOverflow> {
    let room = (n_ctx as usize)
        .checked_sub(prompt_tokens)
        .filter(|r| *r > 0);
    match room {
        Some(room) => Ok(max_tokens.max(1).min(room.min(u32::MAX as usize) as u32)),
        None => Err(ContextOverflow {
            prompt_tokens,
            n_ctx,
        }),
    }
}

/// The configured context (or the default), never beyond what the model
/// was trained on; `trained == 0` means unknown.
pub fn effective_context(configured: Option<u32>, trained: u32) -> u32 {
    let wanted = configured.unwrap_or(DEFAULT_CONTEXT_SIZE);
    if trained > 0 {
        wanted.min(trained)
    } else {
        wanted
    }
}

/// The request's messages with its separate system prompt first.
pub fn chat_messages(params: &LlmParams) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(params.messages.len() + 1);
    if let Some(system) = &params.system {
        out.push(ChatMessage {
            role: "system".into(),
            content: system.clone(),
        });
    }
    out.extend(params.messages.iter().cloned());
    out
}

/// Add BOS only when the model asks for it and the rendered template has
/// not already written it (a double BOS degrades output).
pub fn should_add_bos(model_adds_bos: bool, prompt: &str, bos_text: &str) -> bool {
    model_adds_bos && (bos_text.is_empty() || !prompt.starts_with(bos_text))
}

/// `length` when generation used its whole budget without an end token.
pub fn finish_for(generated: u32, budget: u32, hit_end: bool) -> Finish {
    if !hit_end && generated >= budget {
        Finish::Length
    } else {
        Finish::Stop
    }
}

/// Split a `<think>...</think>` block (if any) from the answer.
pub fn split_reasoning(text: &str) -> (String, Option<String>) {
    match text.split_once("</think>") {
        Some((thought, answer)) => {
            let thought = thought.trim().trim_start_matches("<think>").trim();
            (answer.trim().to_string(), Some(thought.to_string()))
        }
        None => (text.trim().to_string(), None),
    }
}

/// OpenAI `chat.completion` JSON with real token counts.
pub fn completion_json(
    model: &str,
    text: &str,
    prompt_tokens: usize,
    completion_tokens: u32,
    finish: Finish,
    elapsed_ms: u64,
) -> serde_json::Value {
    let (content, reasoning) = split_reasoning(text);
    let mut message = serde_json::json!({ "role": "assistant", "content": content });
    if let Some(reasoning) = reasoning {
        message["reasoning_content"] = reasoning.into();
    }
    serde_json::json!({
        "object": "chat.completion",
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish.as_str() }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens as usize,
        },
        "elapsed_ms": elapsed_ms,
    })
}

/// Releases generated text as it becomes safe to stream: text that could
/// still turn into a stop string is held back until it either does (and is
/// cut) or cannot.
#[derive(Debug)]
pub struct StopHold {
    stops: Vec<String>,
    out: String,
    emitted: usize,
    stopped: Option<usize>,
}

impl StopHold {
    pub fn new(stops: &[String]) -> Self {
        Self {
            stops: stops.iter().filter(|s| !s.is_empty()).cloned().collect(),
            out: String::new(),
            emitted: 0,
            stopped: None,
        }
    }

    /// Add a piece; returns the text now safe to stream.
    pub fn push(&mut self, piece: &str) -> String {
        if self.stopped.is_some() {
            return String::new();
        }
        let search_from = self.emitted;
        self.out.push_str(piece);
        if let Some(at) = self
            .stops
            .iter()
            .filter_map(|s| {
                self.out[search_from..]
                    .find(s.as_str())
                    .map(|i| i + search_from)
            })
            .min()
        {
            self.out.truncate(at);
            self.stopped = Some(at);
            return self.release(at);
        }
        let held = self.held_suffix();
        self.release(self.out.len() - held)
    }

    /// Everything still held (generation ended).
    pub fn finish(&mut self) -> String {
        self.release(self.out.len())
    }

    /// Where a stop string cut the text, if one did.
    pub fn stopped(&self) -> Option<usize> {
        self.stopped
    }

    /// The whole text so far (after any stop cut).
    pub fn text(&self) -> &str {
        &self.out
    }

    /// Length of the longest pending suffix that is a proper prefix of a stop.
    fn held_suffix(&self) -> usize {
        let pending = &self.out[self.emitted..];
        pending
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| {
                let tail = &pending[i..];
                self.stops
                    .iter()
                    .any(|s| s.len() > tail.len() && s.starts_with(tail))
            })
            .map_or(0, |i| pending.len() - i)
    }

    fn release(&mut self, upto: usize) -> String {
        let upto = upto.max(self.emitted);
        let text = self.out[self.emitted..upto].to_string();
        self.emitted = upto;
        text
    }
}

/// A piece of a streamed answer: the model's thinking or the answer itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Reasoning(String),
    Content(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
enum ThinkState {
    /// Not yet known whether the answer opens with a think block.
    #[default]
    Undecided,
    Thinking,
    Answering,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Streams a `<think>...</think>` block (if the answer opens with one) as
/// reasoning and the rest as content, the same split `completion_json`
/// makes on a whole answer.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    state: ThinkState,
    buf: String,
}

impl ThinkSplitter {
    pub fn push(&mut self, piece: &str) -> Vec<Delta> {
        self.buf.push_str(piece);
        let mut out = Vec::new();
        loop {
            match self.state {
                ThinkState::Undecided => {
                    let trimmed = self.buf.trim_start();
                    if let Some(rest) = trimmed.strip_prefix(THINK_OPEN) {
                        self.buf = rest.to_string();
                        self.state = ThinkState::Thinking;
                    } else if THINK_OPEN.starts_with(trimmed) {
                        return out; // could still become <think>
                    } else {
                        self.state = ThinkState::Answering;
                    }
                }
                ThinkState::Thinking => match self.buf.find(THINK_CLOSE) {
                    Some(at) => {
                        let thought = self.buf[..at].trim().to_string();
                        if !thought.is_empty() {
                            out.push(Delta::Reasoning(thought));
                        }
                        self.buf = self.buf[at + THINK_CLOSE.len()..].trim_start().to_string();
                        self.state = ThinkState::Answering;
                    }
                    None => return out, // reasoning is sent whole, at its end
                },
                ThinkState::Answering => {
                    if !self.buf.is_empty() {
                        out.push(Delta::Content(std::mem::take(&mut self.buf)));
                    }
                    return out;
                }
            }
        }
    }

    /// Generation ended: whatever is buffered goes out as what it was.
    pub fn finish(&mut self) -> Vec<Delta> {
        let rest = std::mem::take(&mut self.buf);
        match self.state {
            ThinkState::Thinking if !rest.trim().is_empty() => {
                vec![Delta::Reasoning(rest.trim().to_string())]
            }
            ThinkState::Undecided | ThinkState::Answering if !rest.is_empty() => {
                vec![Delta::Content(rest)]
            }
            _ => Vec::new(),
        }
    }
}

/// One streamed `chat.completion.chunk`.
pub fn chunk_frame(model: &str, delta: &Delta) -> serde_json::Value {
    let delta = match delta {
        Delta::Content(t) => serde_json::json!({ "content": t }),
        Delta::Reasoning(t) => serde_json::json!({ "reasoning_content": t }),
    };
    serde_json::json!({
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": null }],
    })
}

/// The last chunk: why generation stopped, and the token counts.
pub fn final_frame(
    model: &str,
    finish: Finish,
    prompt_tokens: usize,
    completion_tokens: u32,
) -> serde_json::Value {
    serde_json::json!({
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": finish.as_str() }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens as usize,
        },
    })
}

/// A server-sent event carrying `value`.
pub fn sse_event(value: &serde_json::Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

/// The event that ends an OpenAI stream.
pub const SSE_DONE: &[u8] = b"data: [DONE]\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, LlmParams};

    #[test]
    fn budget_is_max_tokens_when_it_fits() {
        assert_eq!(plan_budget(100, 50, 2048), Ok(50));
    }

    #[test]
    fn budget_is_clamped_to_the_room_left_in_the_context() {
        assert_eq!(plan_budget(2000, 100, 2048), Ok(48));
    }

    #[test]
    fn a_prompt_that_fills_the_context_is_refused() {
        let err = plan_budget(2048, 10, 2048).unwrap_err();
        assert_eq!(
            err,
            ContextOverflow {
                prompt_tokens: 2048,
                n_ctx: 2048
            }
        );
        assert_eq!(
            err.to_string(),
            "prompt is 2048 tokens but the context holds 2048; raise contextSize or shorten the prompt"
        );
    }

    #[test]
    fn a_zero_budget_still_generates_one_token() {
        assert_eq!(plan_budget(10, 0, 2048), Ok(1));
    }

    #[test]
    fn context_is_the_configured_size_capped_by_training() {
        assert_eq!(effective_context(Some(32768), 262144), 32768);
        assert_eq!(effective_context(None, 262144), DEFAULT_CONTEXT_SIZE);
        assert_eq!(effective_context(Some(32768), 4096), 4096);
        assert_eq!(
            effective_context(Some(32768), 0),
            32768,
            "unknown training size"
        );
    }

    #[test]
    fn the_system_prompt_leads_the_messages() {
        let params = LlmParams {
            system: Some("be brief".into()),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            ..Default::default()
        };
        let msgs = chat_messages(&params);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "be brief");
        assert_eq!(msgs[1].content, "hi");
        let bare = LlmParams {
            messages: params.messages.clone(),
            ..Default::default()
        };
        assert_eq!(chat_messages(&bare).len(), 1);
    }

    #[test]
    fn bos_is_added_only_when_the_model_wants_it_and_the_template_did_not() {
        assert!(should_add_bos(true, "hello", "<s>"));
        assert!(!should_add_bos(true, "<s>hello", "<s>"));
        assert!(!should_add_bos(false, "hello", "<s>"));
        assert!(
            should_add_bos(true, "hello", ""),
            "an empty BOS text never matches"
        );
    }

    #[test]
    fn finish_is_length_when_the_budget_ran_out() {
        assert_eq!(finish_for(50, 50, false), Finish::Length);
        assert_eq!(finish_for(12, 50, true), Finish::Stop);
        assert_eq!(Finish::Stop.as_str(), "stop");
        assert_eq!(Finish::Length.as_str(), "length");
    }

    #[test]
    fn reasoning_is_split_from_the_answer() {
        assert_eq!(
            split_reasoning("<think>\nhmm\n</think>\n\n{\"a\":1}"),
            ("{\"a\":1}".to_string(), Some("hmm".to_string()))
        );
        assert_eq!(split_reasoning("  plain  "), ("plain".to_string(), None));
        assert_eq!(
            split_reasoning("still thinking</think>"),
            (String::new(), Some("still thinking".to_string()))
        );
    }

    #[test]
    fn completion_json_is_openai_shaped_with_real_counts() {
        let json = completion_json("m", "<think>x</think>answer", 9649, 133, Finish::Stop, 2549);
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "m");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "answer");
        assert_eq!(json["choices"][0]["message"]["reasoning_content"], "x");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["usage"]["prompt_tokens"], 9649);
        assert_eq!(json["usage"]["completion_tokens"], 133);
        assert_eq!(json["usage"]["total_tokens"], 9782);
        assert_eq!(json["elapsed_ms"], 2549);
    }

    #[test]
    fn completion_json_omits_absent_reasoning() {
        let json = completion_json("m", "answer", 1, 1, Finish::Length, 1);
        assert!(json["choices"][0]["message"]
            .get("reasoning_content")
            .is_none());
        assert_eq!(json["choices"][0]["finish_reason"], "length");
    }

    fn drain(hold: &mut StopHold, pieces: &[&str]) -> String {
        let mut out = String::new();
        for p in pieces {
            out.push_str(&hold.push(p));
        }
        out
    }

    #[test]
    fn stop_hold_releases_text_that_cannot_start_a_stop() {
        let mut hold = StopHold::new(&["</s>".to_string()]);
        assert_eq!(hold.push("hello "), "hello ");
        assert_eq!(hold.push("world"), "world");
        assert_eq!(hold.finish(), "");
    }

    #[test]
    fn stop_hold_never_leaks_a_stop_split_across_pieces() {
        let mut hold = StopHold::new(&["END".to_string()]);
        let streamed = drain(&mut hold, &["one E", "N", "D two"]);
        assert_eq!(hold.stopped(), Some("one ".len()));
        assert_eq!(streamed + &hold.finish(), "one ");
    }

    #[test]
    fn stop_hold_without_stops_holds_nothing() {
        let mut hold = StopHold::new(&[]);
        assert_eq!(hold.push("a"), "a");
        assert_eq!(hold.push("é"), "é");
        assert_eq!(hold.stopped(), None);
    }

    #[test]
    fn stop_hold_keeps_multibyte_characters_whole() {
        let mut hold = StopHold::new(&["xyz".to_string()]);
        let mut out = hold.push("ééé");
        out.push_str(&hold.finish());
        assert_eq!(out, "ééé");
    }

    fn split(pieces: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::default();
        let (mut reasoning, mut content) = (String::new(), String::new());
        for p in pieces.iter().copied().map(Some).chain([None]) {
            let deltas = match p {
                Some(p) => s.push(p),
                None => s.finish(),
            };
            for d in deltas {
                match d {
                    Delta::Reasoning(t) => reasoning.push_str(&t),
                    Delta::Content(t) => content.push_str(&t),
                }
            }
        }
        (reasoning, content)
    }

    #[test]
    fn think_splitter_passes_plain_answers_as_content() {
        assert_eq!(
            split(&["{\"a\"", ":1}"]),
            (String::new(), "{\"a\":1}".into())
        );
    }

    #[test]
    fn think_splitter_routes_a_think_block_to_reasoning() {
        assert_eq!(
            split(&["<th", "ink>\nhmm", " ok</th", "ink>\n\nanswer"]),
            ("hmm ok".into(), "answer".into())
        );
    }

    #[test]
    fn think_splitter_treats_text_that_only_looks_like_a_start_as_content() {
        assert_eq!(split(&["<t", "able>"]), (String::new(), "<table>".into()));
        assert_eq!(split(&["<th"]), (String::new(), "<th".into()));
    }

    #[test]
    fn think_splitter_flushes_an_unfinished_think_as_reasoning() {
        assert_eq!(split(&["<think>still"]), ("still".into(), String::new()));
    }

    #[test]
    fn stream_frames_are_openai_chunks() {
        let content = chunk_frame("m", &Delta::Content("hi".into()));
        assert_eq!(
            content,
            serde_json::json!({
                "object": "chat.completion.chunk",
                "model": "m",
                "choices": [{ "index": 0, "delta": { "content": "hi" }, "finish_reason": null }],
            })
        );
        let reasoning = chunk_frame("m", &Delta::Reasoning("r".into()));
        assert_eq!(
            reasoning["choices"][0]["delta"],
            serde_json::json!({ "reasoning_content": "r" })
        );
        let last = final_frame("m", Finish::Length, 10, 3);
        assert_eq!(last["choices"][0]["delta"], serde_json::json!({}));
        assert_eq!(last["choices"][0]["finish_reason"], "length");
        assert_eq!(last["usage"]["total_tokens"], 13);
    }

    #[test]
    fn sse_events_are_data_lines() {
        assert_eq!(
            sse_event(&serde_json::json!({ "a": 1 })),
            b"data: {\"a\":1}\n\n"
        );
        assert_eq!(SSE_DONE, b"data: [DONE]\n\n");
    }
}
