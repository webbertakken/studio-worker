//! The five tabs the UI exposes.  Pure data + tiny enum impl so the
//! contract is testable without egui in scope.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Status,
    Jobs,
    Config,
    Logs,
    About,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Status,
        Tab::Jobs,
        Tab::Config,
        Tab::Logs,
        Tab::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Jobs => "Jobs",
            Tab::Config => "Config",
            Tab::Logs => "Logs",
            Tab::About => "About",
        }
    }
}

impl Default for Tab {
    fn default() -> Self {
        Tab::Status
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
    fn labels_are_unique() {
        use std::collections::HashSet;
        let unique: HashSet<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(unique.len(), Tab::ALL.len());
    }
}
