//! Per-model lifecycle state machine (see `docs/runtime/model-lifecycle.md`).
//!
//! Pure: it decides what the model host must do next, and the host
//! reports back when that work finishes.  Admission and exclusive groups
//! are the host's concern; this only guards one model's transitions.

/// Where one catalogue model is.  Only the worker changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelState {
    Unloaded,
    Loading,
    Loaded,
    Unloading,
    Failed { reason: String },
}

impl ModelState {
    /// The name used on the wire and in logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Unloading => "unloading",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether requests may be served on the model's lane.
    pub fn serves(&self) -> bool {
        matches!(self, Self::Loaded)
    }
}

/// Work the host must start after a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    None,
    BeginLoad,
    BeginUnload,
}

/// A finish event arrived in a state that never started that work.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{event} is unexpected while {state}")]
pub struct UnexpectedEvent {
    pub event: &'static str,
    pub state: &'static str,
}

/// One model's lifecycle.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    state: ModelState,
    /// An unload asked for while loading; runs once the load succeeds.
    unload_pending: bool,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            state: ModelState::Unloaded,
            unload_pending: false,
        }
    }

    pub fn state(&self) -> &ModelState {
        &self.state
    }

    pub fn request_load(&mut self) -> Command {
        match self.state {
            ModelState::Unloaded | ModelState::Failed { .. } => {
                self.state = ModelState::Loading;
                Command::BeginLoad
            }
            ModelState::Loading => {
                self.unload_pending = false;
                Command::None
            }
            ModelState::Loaded | ModelState::Unloading => Command::None,
        }
    }

    pub fn request_unload(&mut self) -> Command {
        match self.state {
            ModelState::Loaded => {
                self.state = ModelState::Unloading;
                Command::BeginUnload
            }
            ModelState::Loading => {
                self.unload_pending = true;
                Command::None
            }
            ModelState::Failed { .. } => {
                self.state = ModelState::Unloaded;
                Command::None
            }
            ModelState::Unloaded | ModelState::Unloading => Command::None,
        }
    }

    pub fn load_finished(
        &mut self,
        outcome: Result<(), String>,
    ) -> Result<Command, UnexpectedEvent> {
        if self.state != ModelState::Loading {
            return Err(self.unexpected("load_finished"));
        }
        let unload_pending = std::mem::take(&mut self.unload_pending);
        match outcome {
            Ok(()) if unload_pending => {
                self.state = ModelState::Unloading;
                Ok(Command::BeginUnload)
            }
            Ok(()) => {
                self.state = ModelState::Loaded;
                Ok(Command::None)
            }
            Err(reason) => {
                self.state = ModelState::Failed { reason };
                Ok(Command::None)
            }
        }
    }

    pub fn unload_finished(
        &mut self,
        outcome: Result<(), String>,
    ) -> Result<Command, UnexpectedEvent> {
        if self.state != ModelState::Unloading {
            return Err(self.unexpected("unload_finished"));
        }
        self.state = match outcome {
            Ok(()) => ModelState::Unloaded,
            Err(reason) => ModelState::Failed { reason },
        };
        Ok(Command::None)
    }

    fn unexpected(&self, event: &'static str) -> UnexpectedEvent {
        UnexpectedEvent {
            event,
            state: self.state.name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> Lifecycle {
        let mut l = Lifecycle::new();
        assert_eq!(l.request_load(), Command::BeginLoad);
        assert_eq!(l.load_finished(Ok(())), Ok(Command::None));
        l
    }

    #[test]
    fn starts_unloaded() {
        assert_eq!(Lifecycle::new().state(), &ModelState::Unloaded);
    }

    #[test]
    fn load_moves_unloaded_to_loading_then_loaded() {
        let mut l = Lifecycle::new();
        assert_eq!(l.request_load(), Command::BeginLoad);
        assert_eq!(l.state(), &ModelState::Loading);
        assert_eq!(l.load_finished(Ok(())), Ok(Command::None));
        assert_eq!(l.state(), &ModelState::Loaded);
    }

    #[test]
    fn load_failure_moves_to_failed_with_the_reason() {
        let mut l = Lifecycle::new();
        l.request_load();
        assert_eq!(l.load_finished(Err("cuda oom".into())), Ok(Command::None));
        assert_eq!(
            l.state(),
            &ModelState::Failed {
                reason: "cuda oom".into()
            }
        );
    }

    #[test]
    fn load_is_a_noop_while_loading_or_loaded() {
        let mut l = Lifecycle::new();
        l.request_load();
        assert_eq!(l.request_load(), Command::None);
        assert_eq!(l.state(), &ModelState::Loading);
        let mut l = loaded();
        assert_eq!(l.request_load(), Command::None);
        assert_eq!(l.state(), &ModelState::Loaded);
    }

    #[test]
    fn load_retries_from_failed() {
        let mut l = Lifecycle::new();
        l.request_load();
        l.load_finished(Err("x".into())).unwrap();
        assert_eq!(l.request_load(), Command::BeginLoad);
        assert_eq!(l.state(), &ModelState::Loading);
    }

    #[test]
    fn unload_moves_loaded_to_unloading_then_unloaded() {
        let mut l = loaded();
        assert_eq!(l.request_unload(), Command::BeginUnload);
        assert_eq!(l.state(), &ModelState::Unloading);
        assert_eq!(l.unload_finished(Ok(())), Ok(Command::None));
        assert_eq!(l.state(), &ModelState::Unloaded);
    }

    #[test]
    fn unload_failure_moves_to_failed() {
        let mut l = loaded();
        l.request_unload();
        l.unload_finished(Err("stuck".into())).unwrap();
        assert_eq!(
            l.state(),
            &ModelState::Failed {
                reason: "stuck".into()
            }
        );
    }

    #[test]
    fn unload_is_a_noop_while_unloaded_or_unloading() {
        let mut l = Lifecycle::new();
        assert_eq!(l.request_unload(), Command::None);
        assert_eq!(l.state(), &ModelState::Unloaded);
        let mut l = loaded();
        l.request_unload();
        assert_eq!(l.request_unload(), Command::None);
        assert_eq!(l.state(), &ModelState::Unloading);
    }

    #[test]
    fn unload_clears_a_failed_model() {
        let mut l = Lifecycle::new();
        l.request_load();
        l.load_finished(Err("x".into())).unwrap();
        assert_eq!(l.request_unload(), Command::None);
        assert_eq!(l.state(), &ModelState::Unloaded);
    }

    #[test]
    fn unload_while_loading_runs_after_the_load_succeeds() {
        let mut l = Lifecycle::new();
        l.request_load();
        assert_eq!(l.request_unload(), Command::None);
        assert_eq!(l.state(), &ModelState::Loading);
        assert_eq!(l.load_finished(Ok(())), Ok(Command::BeginUnload));
        assert_eq!(l.state(), &ModelState::Unloading);
    }

    #[test]
    fn unload_while_loading_is_dropped_when_the_load_fails() {
        let mut l = Lifecycle::new();
        l.request_load();
        l.request_unload();
        assert_eq!(l.load_finished(Err("x".into())), Ok(Command::None));
        assert!(matches!(l.state(), ModelState::Failed { .. }));
    }

    #[test]
    fn load_after_a_pending_unload_cancels_it() {
        let mut l = Lifecycle::new();
        l.request_load();
        l.request_unload();
        assert_eq!(l.request_load(), Command::None);
        assert_eq!(l.load_finished(Ok(())), Ok(Command::None));
        assert_eq!(l.state(), &ModelState::Loaded);
    }

    #[test]
    fn a_finish_event_in_the_wrong_state_is_refused() {
        let mut l = Lifecycle::new();
        assert_eq!(
            l.load_finished(Ok(())),
            Err(UnexpectedEvent {
                event: "load_finished",
                state: "unloaded"
            })
        );
        assert_eq!(
            l.unload_finished(Ok(())),
            Err(UnexpectedEvent {
                event: "unload_finished",
                state: "unloaded"
            })
        );
        assert_eq!(l.state(), &ModelState::Unloaded);
    }

    #[test]
    fn state_names_are_the_wire_names() {
        assert_eq!(ModelState::Unloaded.name(), "unloaded");
        assert_eq!(ModelState::Loading.name(), "loading");
        assert_eq!(ModelState::Loaded.name(), "loaded");
        assert_eq!(ModelState::Unloading.name(), "unloading");
        assert_eq!(ModelState::Failed { reason: "r".into() }.name(), "failed");
    }

    #[test]
    fn only_loaded_serves() {
        assert!(ModelState::Loaded.serves());
        for s in [
            ModelState::Unloaded,
            ModelState::Loading,
            ModelState::Unloading,
            ModelState::Failed { reason: "r".into() },
        ] {
            assert!(!s.serves(), "{} must not serve", s.name());
        }
    }

    #[test]
    fn unexpected_event_displays_event_and_state() {
        let e = UnexpectedEvent {
            event: "load_finished",
            state: "loaded",
        };
        assert_eq!(e.to_string(), "load_finished is unexpected while loaded");
    }
}
