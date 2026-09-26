//! Render a model's own chat template (the GGUF's `tokenizer.chat_template`,
//! Jinja) into a prompt.  Pure, so it is tested without loading a model.
//!
//! Using the model's own template (instead of a generic one) is what
//! makes the model answer in its trained format, and what lets callers
//! pass template switches such as `enable_thinking`.

use crate::types::ChatMessage;
use serde_json::{Map, Value};

/// Special tokens some templates reference (`{{ bos_token }}`).
#[derive(Debug, Clone, Default)]
pub struct TemplateVars {
    pub bos_token: String,
    pub eos_token: String,
}

/// A template that failed to compile or render.
#[derive(Debug, thiserror::Error)]
#[error("chat template: {0}")]
pub struct TemplateError(String);

/// Render `messages` with `template`, ending in the assistant turn.
/// `kwargs` are extra template variables (e.g. `enable_thinking`).
pub fn render_chat(
    template: &str,
    messages: &[ChatMessage],
    kwargs: &Map<String, Value>,
    vars: &TemplateVars,
) -> Result<String, TemplateError> {
    let mut env = minijinja::Environment::new();
    minijinja_contrib::add_to_environment(&mut env);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.add_function(
        "raise_exception",
        |message: String| -> Result<String, minijinja::Error> {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                message,
            ))
        },
    );
    let err = |e: minijinja::Error| {
        // Keep the template's own message (e.g. `raise_exception`) visible.
        let detail = e
            .detail()
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string());
        TemplateError(detail)
    };
    let compiled = env.template_from_str(template).map_err(err)?;
    let mut ctx: Map<String, Value> = kwargs.clone();
    ctx.insert(
        "messages".into(),
        serde_json::to_value(messages).map_err(|e| TemplateError(e.to_string()))?,
    );
    ctx.insert("add_generation_prompt".into(), Value::Bool(true));
    ctx.insert("bos_token".into(), vars.bos_token.clone().into());
    ctx.insert("eos_token".into(), vars.eos_token.clone().into());
    compiled
        .render(minijinja::Value::from_serialize(&ctx))
        .map_err(err)
}

/// Model defaults overlaid with the request's own kwargs (request wins).
pub fn merge_kwargs(
    model: Option<&Map<String, Value>>,
    request: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut merged = model.cloned().unwrap_or_default();
    if let Some(request) = request {
        merged.extend(request.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    /// Trimmed from Qwen3.5's template: thinking only when asked for.
    const QWEN_LIKE: &str = r#"{%- for message in messages %}{{- '<|im_start|>' + message.role + '\n' + message.content + '<|im_end|>' + '\n' }}{%- endfor %}{%- if add_generation_prompt %}{{- '<|im_start|>assistant\n' }}{%- if enable_thinking is defined and enable_thinking is true %}{{- '<think>\n' }}{%- else %}{{- '<think>\n\n</think>\n\n' }}{%- endif %}{%- endif %}"#;

    fn msgs() -> Vec<ChatMessage> {
        vec![
            ChatMessage {
                role: "system".into(),
                content: "Answer in JSON.".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            },
        ]
    }

    fn vars() -> TemplateVars {
        TemplateVars {
            bos_token: "<s>".into(),
            eos_token: "</s>".into(),
        }
    }

    fn kwargs(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn renders_messages_and_the_generation_prompt() {
        let out = render_chat(QWEN_LIKE, &msgs(), &Default::default(), &vars()).unwrap();
        assert_eq!(
            out,
            "<|im_start|>system\nAnswer in JSON.<|im_end|>\n<|im_start|>user\nhi<|im_end|>\n\
             <|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
    }

    #[test]
    fn kwargs_reach_the_template() {
        let out = render_chat(
            QWEN_LIKE,
            &msgs(),
            &kwargs(serde_json::json!({ "enable_thinking": true })),
            &vars(),
        )
        .unwrap();
        assert!(out.ends_with("<|im_start|>assistant\n<think>\n"), "{out}");
    }

    #[test]
    fn bos_and_eos_tokens_are_available() {
        let out = render_chat(
            "{{ bos_token }}{{ messages[0].content }}{{ eos_token }}",
            &msgs(),
            &Default::default(),
            &vars(),
        )
        .unwrap();
        assert_eq!(out, "<s>Answer in JSON.</s>");
    }

    #[test]
    fn python_string_methods_work() {
        let out = render_chat(
            "{% if messages[1].content.startswith('h') %}{{ messages[1].content.upper() }}{% endif %}",
            &msgs(),
            &Default::default(),
            &vars(),
        )
        .unwrap();
        assert_eq!(out, "HI");
    }

    #[test]
    fn raise_exception_becomes_a_named_error() {
        let err = render_chat(
            "{{ raise_exception('System role not supported') }}",
            &msgs(),
            &Default::default(),
            &vars(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("System role not supported"),
            "{err}"
        );
    }

    #[test]
    fn a_broken_template_is_a_named_error() {
        let err = render_chat("{% for %}", &msgs(), &Default::default(), &vars()).unwrap_err();
        assert!(err.to_string().starts_with("chat template"), "{err}");
    }

    #[test]
    fn request_kwargs_override_model_defaults() {
        let merged = merge_kwargs(
            Some(&kwargs(
                serde_json::json!({ "enable_thinking": false, "a": 1 }),
            )),
            Some(&kwargs(serde_json::json!({ "enable_thinking": true }))),
        );
        assert_eq!(
            serde_json::Value::Object(merged),
            serde_json::json!({ "enable_thinking": true, "a": 1 })
        );
        assert!(merge_kwargs(None, None).is_empty());
    }
}
