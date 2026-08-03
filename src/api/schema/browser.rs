use serde::{Deserialize, Serialize};

/// Opens a Browser pane split from `pane_id` (or the focused pane if
/// omitted). MVP: no direction/ratio/focus controls yet -- always splits
/// right of the target at a 0.5 ratio and focuses the new pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrowserOpenParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrowserNavigateParams {
    pub pane_id: String,
    pub url: String,
}

/// Identifies a Browser pane for an action that needs no other input
/// (`browser.reload`, `browser.back`, `browser.forward`, `browser.close`,
/// `browser.info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrowserPaneTarget {
    pub pane_id: String,
}

/// What a Browser pane is currently showing. `url` and `title` are whatever
/// the last successful page-info poll observed, so they lag a navigation by
/// up to one poll interval and are absent until the first one lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrowserPageInfo {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Set when the pane's `agent-browser` session has failed and the pane is
    /// waiting for a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
