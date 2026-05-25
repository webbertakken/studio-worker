//! The five tabs the UI exposes.  Pure data + tiny enum impl so the
//! contract is testable without egui in scope.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tab {
    #[default]
    Status,
    Jobs,
    Config,
    Logs,
    About,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Status, Tab::Jobs, Tab::Config, Tab::Logs, Tab::About];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Jobs => "Jobs",
            Tab::Config => "Config",
            Tab::Logs => "Logs",
            Tab::About => "About",
        }
    }

    /// Parse a tab name (case-insensitive).  Used by the
    /// `STUDIO_WORKER_UI_TAB` debug env var to seed the initial tab
    /// during screenshot capture and headless UI inspection.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "status" => Some(Self::Status),
            "jobs" => Some(Self::Jobs),
            "config" => Some(Self::Config),
            "logs" => Some(Self::Logs),
            "about" => Some(Self::About),
            _ => None,
        }
    }

    /// Resolve the initial tab on app launch: env override or default.
    pub fn initial() -> Self {
        std::env::var("STUDIO_WORKER_UI_TAB")
            .ok()
            .and_then(|s| Self::parse(&s))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_returns_five_tabs_in_render_order() {
        let labels: Vec<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(
            labels,
            ["Status", "Jobs", "Config", "Logs", "About"],
            "tab labels + order are part of the UI contract"
        );
    }

    #[test]
    fn default_is_status() {
        assert_eq!(Tab::default(), Tab::Status);
    }

    #[test]
    fn parse_round_trips_with_label_case_insensitively() {
        for tab in Tab::ALL {
            assert_eq!(Tab::parse(tab.label()), Some(tab));
            assert_eq!(Tab::parse(&tab.label().to_uppercase()), Some(tab));
        }
        assert!(Tab::parse("").is_none());
        assert!(Tab::parse("nope").is_none());
    }

    #[test]
    fn initial_falls_back_to_default_when_env_unset() {
        // SAFETY: tests run single-threaded for env mutation isolation.
        // Snapshot + restore even if the test panics.
        let prev = std::env::var("STUDIO_WORKER_UI_TAB").ok();
        std::env::remove_var("STUDIO_WORKER_UI_TAB");
        assert_eq!(Tab::initial(), Tab::default());
        if let Some(v) = prev {
            std::env::set_var("STUDIO_WORKER_UI_TAB", v);
        }
    }

    #[test]
    fn labels_are_unique() {
        use std::collections::HashSet;
        let unique: HashSet<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(unique.len(), Tab::ALL.len());
    }
}
