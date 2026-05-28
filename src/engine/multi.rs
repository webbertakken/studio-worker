//! Composite engine that delegates per (kind, model).
//!
//! The worker no longer has an `engine` knob in its config: instead,
//! `engine::build()` returns a `MultiEngine` populated with every
//! backend compiled into the binary (synthetic always; llama /
//! whisper / image-candle / video / tts when their cargo features
//! are on).
//!
//! For each incoming job [`MultiEngine`] picks the first engine in
//! the list that advertises support for the requested model.  If no
//! engine claims the exact model it falls back to the first engine
//! that handles the task kind at all.  Real backends are inserted
//! ahead of synthetic so they win when both could serve the same
//! (kind, model).  If nothing matches the dispatch fails with the
//! "cannot serve <kind>" shape the studio's claim loop already
//! knows how to handle.
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use tracing::{debug, warn};

/// Tracing target for the multi engine.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::engine::multi=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::multi";

pub struct MultiEngine {
    engines: Vec<Box<dyn Engine>>,
}

impl MultiEngine {
    pub fn new(engines: Vec<Box<dyn Engine>>) -> Self {
        Self { engines }
    }

    fn pick_for(&self, kind: TaskKind, model: &str) -> Option<&dyn Engine> {
        for e in &self.engines {
            if e.capabilities().supports(kind, model) {
                debug!(
                    target: TRACE_TARGET,
                    op = "pick",
                    kind = kind.as_str(),
                    model,
                    sub_engine = e.name(),
                    r#match = "exact",
                    "engine selected"
                );
                return Some(e.as_ref());
            }
        }
        // Fallback: any engine that advertises the kind at all.
        for e in &self.engines {
            if e.capabilities()
                .supported_models_per_kind
                .contains_key(&kind)
            {
                debug!(
                    target: TRACE_TARGET,
                    op = "pick",
                    kind = kind.as_str(),
                    model,
                    sub_engine = e.name(),
                    r#match = "fallback",
                    "engine selected by kind fallback"
                );
                return Some(e.as_ref());
            }
        }
        warn!(
            target: TRACE_TARGET,
            op = "pick",
            kind = kind.as_str(),
            model,
            "no engine advertises this kind"
        );
        None
    }
}

impl Engine for MultiEngine {
    fn name(&self) -> &'static str {
        "multi"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        for e in &self.engines {
            for (kind, models) in e.capabilities().supported_models_per_kind {
                let entry = map.entry(kind).or_default();
                for m in models {
                    if !entry.contains(&m) {
                        entry.push(m);
                    }
                }
            }
        }
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let Some(engine) = self.pick_for(kind, model) else {
            bail!("multi engine cannot serve {} tasks", kind.as_str());
        };
        engine.dispatch(model, task)
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &crate::types::ModelSource,
    ) -> Result<TaskResult> {
        let kind = task.kind();
        // The studio knows exactly which engine should serve this
        // job (source.engine); we route strictly to that backend.
        // No silent fallback to synthetic for real-model offers —
        // see DECISIONS.md "Synthetic fallback removed for real
        // models".
        let wanted = match source.engine {
            crate::types::ModelEngine::SdCpp => "sdcpp",
            crate::types::ModelEngine::LlamaCpp => "llama",
            crate::types::ModelEngine::Synthetic => "synthetic",
        };
        for e in &self.engines {
            if e.name() == wanted {
                debug!(
                    target: TRACE_TARGET,
                    op = "pick",
                    kind = kind.as_str(),
                    model,
                    sub_engine = e.name(),
                    r#match = "model-source",
                    "engine selected by ModelSource.engine"
                );
                return e.dispatch_with_source(model, task, source);
            }
        }
        bail!(
            "no `{}` engine compiled into this worker (model {} requires it)",
            wanted,
            model
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SyntheticEngine;

    struct StubEngine {
        name: &'static str,
        kinds: Vec<TaskKind>,
        models: Vec<String>,
    }

    impl Engine for StubEngine {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> EngineCapabilities {
            let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
            for k in &self.kinds {
                map.insert(*k, self.models.clone());
            }
            EngineCapabilities {
                supported_models_per_kind: map,
            }
        }
        fn dispatch(&self, _model: &str, task: Task) -> Result<TaskResult> {
            // Return a sentinel result tagged with the engine name so we
            // can verify routing in tests.
            match task {
                Task::Image(_) => Ok(TaskResult::Image {
                    bytes: self.name.as_bytes().to_vec(),
                    ext: "test".into(),
                }),
                Task::Llm(_) => Ok(TaskResult::Llm {
                    json: serde_json::json!({ "from": self.name }),
                }),
                _ => bail!("stub doesn't serve this"),
            }
        }
    }

    fn image_task() -> Task {
        Task::Image(ImageParams {
            prompt: "x".into(),
            width: 64,
            height: 64,
            steps: 1,
            ext: "webp".into(),
            ..Default::default()
        })
    }

    fn llm_task() -> Task {
        Task::Llm(LlmParams {
            messages: vec![],
            max_tokens: 1,
            temperature: 0.0,
            ..Default::default()
        })
    }

    #[test]
    fn multi_picks_first_engine_supporting_the_kind_and_model() {
        let a: Box<dyn Engine> = Box::new(StubEngine {
            name: "a",
            kinds: vec![TaskKind::Image],
            models: vec!["alpha".into()],
        });
        let b: Box<dyn Engine> = Box::new(StubEngine {
            name: "b",
            kinds: vec![TaskKind::Image],
            models: vec!["beta".into()],
        });
        let multi = MultiEngine::new(vec![a, b]);

        let result = multi.dispatch("alpha", image_task()).unwrap();
        match result {
            TaskResult::Image { bytes, .. } => assert_eq!(bytes, b"a"),
            _ => panic!("expected image"),
        }
        let result = multi.dispatch("beta", image_task()).unwrap();
        match result {
            TaskResult::Image { bytes, .. } => assert_eq!(bytes, b"b"),
            _ => panic!("expected image"),
        }
    }

    #[test]
    fn multi_falls_back_to_first_engine_advertising_the_kind() {
        let alpha_only: Box<dyn Engine> = Box::new(StubEngine {
            name: "alpha",
            kinds: vec![TaskKind::Image],
            models: vec!["alpha-image".into()],
        });
        let llm_only: Box<dyn Engine> = Box::new(StubEngine {
            name: "llm",
            kinds: vec![TaskKind::Llm],
            models: vec!["llama-some".into()],
        });
        let multi = MultiEngine::new(vec![alpha_only, llm_only]);

        let result = multi.dispatch("unknown-model", llm_task()).unwrap();
        match result {
            TaskResult::Llm { json } => assert_eq!(json["from"], "llm"),
            _ => panic!("expected llm"),
        }
    }

    #[test]
    fn multi_errors_when_no_engine_serves_kind() {
        let image_only: Box<dyn Engine> = Box::new(StubEngine {
            name: "image",
            kinds: vec![TaskKind::Image],
            models: vec!["x".into()],
        });
        let multi = MultiEngine::new(vec![image_only]);
        let err = multi.dispatch("x", llm_task()).unwrap_err();
        assert!(err.to_string().contains("cannot serve llm"));
    }

    #[test]
    fn capabilities_union_across_all_engines() {
        let img: Box<dyn Engine> = Box::new(SyntheticEngine::new());
        let stub: Box<dyn Engine> = Box::new(StubEngine {
            name: "extra",
            kinds: vec![TaskKind::Image],
            models: vec!["extra-image-model".into()],
        });
        let multi = MultiEngine::new(vec![img, stub]);
        let caps = multi.capabilities();
        let image = &caps.supported_models_per_kind[&TaskKind::Image];
        assert!(image.contains(&"synthetic".to_string()));
        assert!(image.contains(&"extra-image-model".to_string()));
    }

    #[test]
    fn name_is_multi() {
        let multi = MultiEngine::new(vec![]);
        assert_eq!(multi.name(), "multi");
    }
}
