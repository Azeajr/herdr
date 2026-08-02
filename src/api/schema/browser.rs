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
