//! The pages on the navigation rail.  Pure data so the contract is
//! testable without egui in scope.

/// Tracing-free, egui-free: which page the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Page {
    /// What runs and what ran: the page the window opens on.
    #[default]
    Jobs,
    Models,
    /// The worker's state, registration, hardware and version.
    Worker,
    Logs,
    Config,
}

impl Page {
    /// Rail order; `Ctrl+1` … `Ctrl+5` follow it.
    pub const ALL: [Page; 5] = [
        Page::Jobs,
        Page::Models,
        Page::Worker,
        Page::Logs,
        Page::Config,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Page::Jobs => "Jobs",
            Page::Models => "Models",
            Page::Worker => "Worker",
            Page::Logs => "Logs",
            Page::Config => "Config",
        }
    }

    /// One line for the rail's hover text.
    pub fn hint(self) -> &'static str {
        match self {
            Page::Jobs => "What runs now and what ran",
            Page::Models => "Load and unload models",
            Page::Worker => "State, registration, hardware and version",
            Page::Logs => "Everything the daemon logs",
            Page::Config => "Worker settings and this window",
        }
    }

    /// The page `Ctrl+<digit>` picks (1-based).
    pub fn from_digit(digit: u8) -> Option<Self> {
        Self::ALL.get(usize::from(digit).checked_sub(1)?).copied()
    }

    /// 1-based position on the rail.
    pub fn digit(self) -> u8 {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as u8 + 1
    }

    /// Parse a page name (case-insensitive), for `STUDIO_WORKER_UI_PAGE`.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(name))
    }

    /// The page on open: `STUDIO_WORKER_UI_PAGE` (screenshots, headless
    /// inspection) or [`Page::Jobs`].
    pub fn initial() -> Self {
        Self::initial_from(std::env::var("STUDIO_WORKER_UI_PAGE").ok().as_deref())
    }

    pub fn initial_from(env: Option<&str>) -> Self {
        env.and_then(Self::parse).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rail_lists_every_page_in_order() {
        let labels: Vec<&str> = Page::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(labels, ["Jobs", "Models", "Worker", "Logs", "Config"]);
    }

    #[test]
    fn the_window_opens_on_jobs() {
        assert_eq!(Page::default(), Page::Jobs);
        assert_eq!(Page::initial_from(None), Page::Jobs);
        assert_eq!(Page::initial_from(Some("nonsense")), Page::Jobs);
        assert_eq!(Page::initial_from(Some("worker")), Page::Worker);
    }

    #[test]
    fn names_parse_case_insensitively_and_old_tabs_are_gone() {
        for page in Page::ALL {
            assert_eq!(Page::parse(page.label()), Some(page));
            assert_eq!(Page::parse(&page.label().to_uppercase()), Some(page));
            assert!(!page.hint().is_empty());
        }
        assert_eq!(Page::parse(" logs "), Some(Page::Logs));
        assert_eq!(Page::parse("status"), None);
        assert_eq!(Page::parse("about"), None);
    }

    #[test]
    fn digits_pick_pages_in_rail_order() {
        for page in Page::ALL {
            assert_eq!(Page::from_digit(page.digit()), Some(page));
        }
        assert_eq!(Page::from_digit(1), Some(Page::Jobs));
        assert_eq!(Page::from_digit(0), None);
        assert_eq!(Page::from_digit(6), None);
    }
}
