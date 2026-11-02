//! The parts of `session.snapshot` the TUI reads (herdr: SessionSnapshot, WorkspaceInfo, PaneInfo).
//! Everything else in the payload is ignored, and unknown statuses fold into Unknown, so a newer
//! herdr does not break the list.
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    #[serde(other)]
    Unknown,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Done => "done",
            Status::Unknown => "unknown",
        }
    }
    /// Blocked (a question) or done (unseen result): the agent waits for a person.
    pub fn needs_you(self) -> bool {
        matches!(self, Status::Blocked | Status::Done)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    /// The detected agent (claude, codex, pi, …); None for a plain shell.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub agent_status: Status,
}

impl Pane {
    /// Where the foreground process is, else where the shell started (the board does the same).
    pub fn dir(&self) -> &str {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref()).unwrap_or("")
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub panes: Vec<Pane>,
}

/// From a `session.snapshot` result (`{"type":"session_snapshot","snapshot":{…}}`), or the bare
/// snapshot object, as `herdr api snapshot` fixtures hold it.
pub fn parse_snapshot(result: &Value) -> Result<Snapshot, String> {
    let inner = if result.get("snapshot").is_some() { &result["snapshot"] } else { result };
    serde_json::from_value(inner.clone()).map_err(|e| format!("snapshot: {e}"))
}

#[cfg(test)]
pub const FIXTURE: &str = include_str!("../../tests/fixtures/snapshot.json");

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Snapshot {
        let v: Value = serde_json::from_str(FIXTURE).unwrap();
        parse_snapshot(&v["result"]).unwrap()
    }

    #[test]
    fn parses_the_cli_shape() {
        let s = fixture();
        assert_eq!(s.workspaces.len(), 4);
        assert_eq!(s.panes.len(), 5);
        let blocked = s.panes.iter().find(|p| p.pane_id == "w2:p1").unwrap();
        assert_eq!(blocked.agent_status, Status::Blocked);
        assert_eq!(blocked.agent.as_deref(), Some("codex"));
    }

    #[test]
    fn dir_falls_back_to_cwd_and_unknown_status_is_tolerated() {
        let s = fixture();
        let shell = s.panes.iter().find(|p| p.pane_id == "w4:p1").unwrap();
        assert_eq!(shell.dir(), "/home/dora");
        assert!(shell.agent.is_none());
        let v: Value = serde_json::json!({"workspaces": [], "panes": [{"pane_id": "w9:p1", "workspace_id": "w9", "agent_status": "napping"}]});
        assert_eq!(parse_snapshot(&v).unwrap().panes[0].agent_status, Status::Unknown);
    }
}
