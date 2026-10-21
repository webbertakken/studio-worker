//! The llama process's wire format: one JSON object per line, tagged by `type`, camelCase.
//! Spec: docs/engines/llama.md, "Protocol".

use crate::types::{LlmParams, ModelCliDefaults};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a llama process loads: the files the daemon downloaded, and the model's defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSpec {
    /// The catalogue model id, for logs and the completion's `model`.
    pub model: String,
    pub model_path: PathBuf,
    /// The vision projector (`mmproj`), when the model reads images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projector_path: Option<PathBuf>,
    #[serde(default)]
    pub defaults: ModelCliDefaults,
}

/// A line the daemon writes to the process's stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToProcess {
    /// The first line: load these files.
    Load(LoadSpec),
    /// One chat completion.
    Chat { params: LlmParams },
    /// The model's token ids for `text`.
    #[serde(rename_all = "camelCase")]
    Tokenize { text: String, add_special: bool },
    /// Stop the request in progress.
    Cancel,
}

/// A line the process writes to its stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum FromProcess {
    /// The model is loaded; `version` is the process's crate version.
    Loaded { version: String },
    /// Answer text as it is generated.
    Piece { text: String },
    /// The `chat.completion` JSON of the whole answer.
    Done { completion: serde_json::Value },
    /// The token ids a `tokenize` asked for.
    Tokens { tokens: Vec<i32> },
    /// The load or the request failed; `permanent`: it would fail on any worker too.
    Failed {
        error: String,
        #[serde(default)]
        permanent: bool,
    },
    /// One tracing event of the process: its message, and its other fields as `name=value`.
    Log {
        level: String,
        target: String,
        message: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        fields: String,
    },
}

/// One protocol line, newline included.
pub fn encode<T: Serialize>(message: &T) -> String {
    let mut line = serde_json::to_string(message).expect("protocol messages always serialise");
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_are_tagged_camel_case_lines() {
        let load = ToProcess::Load(LoadSpec {
            model: "qwen3.5-2b".into(),
            model_path: "/m/q.gguf".into(),
            projector_path: None,
            defaults: ModelCliDefaults::default(),
        });
        let line = encode(&load);
        assert!(line.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "load");
        assert_eq!(value["modelPath"], "/m/q.gguf");
        assert!(value.get("projectorPath").is_none());

        let tokenize = encode(&ToProcess::Tokenize {
            text: "hi".into(),
            add_special: true,
        });
        let value: serde_json::Value = serde_json::from_str(&tokenize).unwrap();
        assert_eq!(
            value,
            json!({"type": "tokenize", "text": "hi", "addSpecial": true})
        );
        assert_eq!(encode(&ToProcess::Cancel), "{\"type\":\"cancel\"}\n");
    }

    #[test]
    fn replies_round_trip() {
        for reply in [
            FromProcess::Loaded {
                version: "1.2.3".into(),
            },
            FromProcess::Piece { text: "he".into() },
            FromProcess::Done {
                completion: json!({"object": "chat.completion"}),
            },
            FromProcess::Tokens { tokens: vec![1, 2] },
            FromProcess::Failed {
                error: "boom".into(),
                permanent: true,
            },
            FromProcess::Log {
                level: "ERROR".into(),
                target: "llama-cpp-2".into(),
                message: "cudaMalloc failed: out of memory".into(),
                fields: " module=\"ggml::alloc\"".into(),
            },
        ] {
            let back: FromProcess = serde_json::from_str(&encode(&reply)).unwrap();
            assert_eq!(back, reply);
        }
    }

    #[test]
    fn a_chat_request_carries_the_params() {
        let line = encode(&ToProcess::Chat {
            params: LlmParams {
                max_tokens: 7,
                ..Default::default()
            },
        });
        match serde_json::from_str::<ToProcess>(&line).unwrap() {
            ToProcess::Chat { params } => assert_eq!(params.max_tokens, 7),
            other => panic!("expected chat, got {other:?}"),
        }
    }
}
