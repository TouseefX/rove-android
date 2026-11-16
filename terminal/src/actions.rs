use crate::editor::Editor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Overview,
    Console,
    Network,
}
impl View {
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Console => "console",
            Self::Network => "network",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Search,
    View(View),
    Theme,
    Window,
    Filter,
    Physics,
    Lightweight,
    Sandbox,
    Load,
    Export,
    Clear,
    Leave,
    Help,
    Quit,
}
pub struct Item {
    pub name: &'static str,
    pub hint: &'static str,
    pub key: &'static str,
    pub action: Action,
}
pub const ITEMS: &[Item] = &[
    Item {
        name: "Find a game",
        hint: "Search Roblox or enter a place ID",
        key: "F2",
        action: Action::Search,
    },
    Item {
        name: "Overview",
        hint: "Memory, session, and recent activity",
        key: "Alt+1",
        action: Action::View(View::Overview),
    },
    Item {
        name: "Focus console",
        hint: "More room for Luau and output",
        key: "Alt+2",
        action: Action::View(View::Console),
    },
    Item {
        name: "Network view",
        hint: "Transport RTT and real ping counters",
        key: "Alt+3",
        action: Action::View(View::Network),
    },
    Item {
        name: "Switch theme",
        hint: "Aurora, orchid, midnight, ember, or glacier",
        key: "Ctrl+T",
        action: Action::Theme,
    },
    Item {
        name: "Change graph window",
        hint: "30 seconds, 2 minutes, or 10 minutes",
        key: "F8",
        action: Action::Window,
    },
    Item {
        name: "Filter activity",
        hint: "All, script output, or problems",
        key: "Ctrl+E",
        action: Action::Filter,
    },
    Item {
        name: "Run a script file",
        hint: "Put a file path after :run",
        key: ":run",
        action: Action::Load,
    },
    Item {
        name: "Export memory samples",
        hint: "Save collected measurements to CSV",
        key: ":save",
        action: Action::Export,
    },
    Item {
        name: "Local sandbox",
        hint: "Try Luau without joining Roblox",
        key: ":sandbox",
        action: Action::Sandbox,
    },
    Item {
        name: "Toggle physics",
        hint: "Applies to the next session",
        key: "F3",
        action: Action::Physics,
    },
    Item {
        name: "Toggle lightweight mode",
        hint: "Applies to the next session",
        key: "F4",
        action: Action::Lightweight,
    },
    Item {
        name: "Clear activity",
        hint: "Clear the visible log",
        key: "Ctrl+L",
        action: Action::Clear,
    },
    Item {
        name: "Leave session",
        hint: "Disconnect the current client",
        key: "Ctrl+D",
        action: Action::Leave,
    },
    Item {
        name: "Keyboard guide",
        hint: "See all controls",
        key: "F1",
        action: Action::Help,
    },
    Item {
        name: "Quit Rove",
        hint: "Leave the session and close the terminal UI",
        key: "Ctrl+Q",
        action: Action::Quit,
    },
];
#[derive(Default)]
pub struct Palette {
    pub input: Editor,
    pub selected: usize,
}
pub fn matching(query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    ITEMS
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            let haystack = format!("{} {}", item.name, item.hint).to_lowercase();
            let mut chars = haystack.chars();
            query
                .chars()
                .filter(|c| !c.is_whitespace())
                .all(|needle| chars.by_ref().any(|c| c == needle))
        })
        .map(|(i, _)| i)
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palette_accepts_short_fuzzy_queries_and_empty_results() {
        assert!(matching("gm")
            .iter()
            .any(|i| ITEMS[*i].action == Action::Search));
        assert!(matching("export")
            .iter()
            .any(|i| ITEMS[*i].action == Action::Export));
        assert!(matching("zzzzzzzz").is_empty());
    }
}
