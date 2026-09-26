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
}
