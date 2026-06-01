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

    /// Pick the engine that claims `(kind, model)` exactly.  No
    /// kind-only fallback — the studio's `ModelSource` is
    /// authoritative.  A model whose engine isn't on this worker is
    /// rejected loudly so the operator sees what's missing instead of
    /// silently routing through synthetic placeholder bytes.
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
        warn!(
            target: TRACE_TARGET,
            op = "pick",
            kind = kind.as_str(),
            model,
            "no engine claims this exact (kind, model) pair"
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
            bail!(
                "no engine on this worker can serve model {} (kind={}); \
                 synthetic fallback is disabled",
                model,
                kind.as_str()
            );
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
        warn!(
            target: TRACE_TARGET,
            op = "pick",
            kind = kind.as_str(),
            model,
            sub_engine = wanted,
            r#match = "model-source",
            "requested engine not compiled into this worker"
        );
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
    fn multi_refuses_unknown_model_without_kind_fallback() {
        // An LLM engine is present, but no engine claims the
        // specific model id.  Per the no-fallback policy the
        // dispatch errors loudly instead of routing to the first
        // engine that advertises the kind.
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

        let err = multi.dispatch("unknown-model", llm_task()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no engine on this worker can serve model"),
            "expected no-fallback error, got: {msg}"
        );
        assert!(msg.contains("unknown-model"));
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
        let msg = err.to_string();
        assert!(
            msg.contains("no engine on this worker can serve model"),
            "expected no-fallback error, got: {msg}"
        );
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

    fn sd_cpp_source() -> crate::types::ModelSource {
        crate::types::ModelSource {
            engine: crate::types::ModelEngine::SdCpp,
            files: vec![],
            cli_defaults: crate::types::ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 8,
                width: 1024,
                height: 1024,
                sampling_method: None,
            },
        }
    }

    /// The no-fallback policy: when the studio asks for an `sd-cpp`
    /// model but no sd-cpp engine is compiled in (e.g. CI / minimal
    /// build), dispatch errors loudly instead of silently routing the
    /// job to synthetic.
    #[test]
    fn dispatch_with_source_refuses_to_fall_back_to_synthetic_for_real_models() {
        let synth: Box<dyn Engine> = Box::new(SyntheticEngine::new());
        let multi = MultiEngine::new(vec![synth]);
        let source = sd_cpp_source();
        let err = multi
            .dispatch_with_source("some-real-flux-model", image_task(), &source)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no `sdcpp` engine compiled"),
            "expected no-sdcpp-backend error, got: {err}"
        );
    }

    /// The no-match path of `dispatch_with_source` must emit a
    /// structured breadcrumb on the `studio_worker::engine::multi`
    /// target, symmetric with `pick_for`'s no-match `warn!`.  Without
    /// it, an operator filtering `RUST_LOG=studio_worker::engine::multi`
    /// to trace routing would see "engine selected" events but never
    /// the rejections, making a wrong-engine offer impossible to
    /// diagnose from the routing breadcrumbs alone.
    #[test]
    fn dispatch_with_source_warns_when_wanted_engine_missing() {
        let logs = crate::test_support::capture(|| {
            let synth: Box<dyn Engine> = Box::new(SyntheticEngine::new());
            let multi = MultiEngine::new(vec![synth]);
            let source = sd_cpp_source();
            let _ = multi.dispatch_with_source("some-real-flux-model", image_task(), &source);
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(
            logs.contains("studio_worker::engine::multi"),
            "expected multi target, got: {logs}"
        );
        assert!(logs.contains("op=\"pick\""), "expected op field: {logs}");
        assert!(
            logs.contains("sdcpp"),
            "expected wanted engine name in breadcrumb: {logs}"
        );
        assert!(
            logs.contains("some-real-flux-model"),
            "expected model id in breadcrumb: {logs}"
        );
    }

    /// Synthetic offers (engine == Synthetic) still route to the
    /// synthetic engine.  This is *not* a fallback — the studio
    /// explicitly asked for it.
    #[test]
    fn dispatch_with_source_routes_synthetic_engine_for_synthetic_models() {
        let synth: Box<dyn Engine> = Box::new(SyntheticEngine::new());
        let multi = MultiEngine::new(vec![synth]);
        let source = crate::types::ModelSource {
            engine: crate::types::ModelEngine::Synthetic,
            files: vec![],
            cli_defaults: crate::types::ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 8,
                width: 1024,
                height: 1024,
                sampling_method: None,
            },
        };
        let result = multi
            .dispatch_with_source("synthetic", image_task(), &source)
            .unwrap();
        assert!(matches!(result, TaskResult::Image { .. }));
    }
}
