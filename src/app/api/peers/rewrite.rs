//! Restating a peer's answers in this server's own terms.
//!
//! Free functions over JSON, depending on nothing. A peer answers about its own
//! session, so its ids name its workspaces, tabs and panes; the caller asked
//! about this server and has to be able to use what it gets back. Only ids are
//! ever rewritten — never the text a peer reported.

use super::*;

/// Reads the id of the pane a peer created, or why it did not create one.
///
/// A peer error is reported as-is: its code and message describe what went
/// wrong on the machine that owns the pane, and inventing a local code here
/// would hide that.
pub(super) fn peer_split_pane_id(
    value: &serde_json::Value,
    handle: &PeerHandle,
) -> Result<String, (String, String)> {
    peer_pane_id_at(value, "pane", handle, "the split")
}

/// The peer-side workspace id a peer-side public pane id belongs to.
///
/// Peer ids are the peer's own local ids, so a pane id carries its workspace in
/// the same `<workspace>:<pane>` shape this server uses. `None` for a target
/// that is not a pane id — a terminal id or an agent name names no workspace,
/// and guessing one from it would be wrong rather than merely unknown.
pub(super) fn peer_workspace_of_pane_id(target: &str) -> Option<String> {
    let (workspace, pane) = target.split_once(':')?;
    // Decoded with the inverse of the encoder that produced it, not with a
    // digit test: public pane numbers are base32 over `PUBLIC_ID_ALPHABET`, so
    // the tenth pane of a workspace is `pA`, not `p10`. Reading those as "not a
    // pane" left `peer_workspace` unset and disabled `tab.create` and every
    // `worktree.*` action on the view, with a message saying it named no
    // workspace when it plainly did.
    let is_pane = pane.strip_prefix('p').is_some_and(|number| {
        // An empty number decodes to `Some(0)`, and `p` alone is not an id.
        !number.is_empty() && crate::workspace::decode_public_number(number).is_some()
    });
    (is_pane && !workspace.is_empty()).then(|| workspace.to_string())
}

/// Reads a pane id out of a peer's reply, or why there is none.
///
/// A peer error is reported as-is: its code and message describe what went
/// wrong on the machine that owns the pane, and inventing a local code here
/// would hide that.
pub(super) fn peer_pane_id_at(
    value: &serde_json::Value,
    field: &str,
    handle: &PeerHandle,
    what: &str,
) -> Result<String, (String, String)> {
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unavailable")
            .to_string();
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("peer rejected {what}"));
        return Err((code, message));
    }
    value
        .get("result")
        .and_then(|result| result.get(field))
        .and_then(|pane| pane.get("pane_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "unavailable".to_string(),
                format!("peer '{handle}' returned no pane id for {what}"),
            )
        })
}

/// Rewrites a peer's read response so it names this server's pane.
///
/// Unlike [`rewrite_forwarded_response`], the ids are *replaced* rather than
/// namespaced. A namespaced peer id would be honest about where the screen came
/// from but useless to the caller, who asked about a local pane and will use the
/// id it gets back against this server.
pub(super) fn rewrite_forwarded_read(
    value: &mut serde_json::Value,
    request_id: &str,
    local_ids: &LocalPaneIds,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    let Some(read) = obj
        .get_mut("result")
        .and_then(|result| result.get_mut("read"))
        .and_then(|read| read.as_object_mut())
    else {
        return;
    };
    for (field, local) in [
        ("pane_id", &local_ids.pane_id),
        ("workspace_id", &local_ids.workspace_id),
        ("tab_id", &local_ids.tab_id),
    ] {
        if read.contains_key(field) {
            read.insert(field.to_string(), serde_json::Value::String(local.clone()));
        }
    }
}

/// Rewrites a peer's explain response to answer this server's request and to
/// name the peer that produced it.
///
/// `peer` and `peer_pane_id` go inside the explain body rather than beside it so
/// they travel with the manifest and rule fields they qualify: a client holding
/// only `result.explain` still knows whose rules it has.
///
/// A refusal is stamped too. The peer's message names the peer's own pane id,
/// which means nothing on this side until it says whose pane it is.
pub(super) fn rewrite_forwarded_explain(
    value: &mut serde_json::Value,
    request_id: &str,
    peer: &PeerHandle,
    peer_pane_id: &str,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    if let Some(error) = obj.get_mut("error").and_then(|error| error.as_object_mut()) {
        if let Some(serde_json::Value::String(message)) = error.get("message") {
            let restated = format!("peer '{peer}': {message}");
            error.insert("message".to_string(), serde_json::Value::String(restated));
        }
        return;
    }
    let Some(explain) = obj
        .get_mut("result")
        .and_then(|result| result.get_mut("explain"))
        .and_then(|explain| explain.as_object_mut())
    else {
        return;
    };
    explain.insert(
        "peer".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    explain.insert(
        "peer_pane_id".to_string(),
        serde_json::Value::String(peer_pane_id.to_string()),
    );
}

/// Rewrites a peer's raw response so it reads as this server's own: the response
/// id becomes the caller's request id, and any workspace ids in the result are
/// re-namespaced with the peer's instance id — the same prefixing enumeration
/// applies on ingest, so a client that acts on the returned id can route it back
/// to the same peer.
pub(super) fn rewrite_forwarded_response(
    value: &mut serde_json::Value,
    request_id: &str,
    instance_id: &str,
    remap_ids: bool,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    if !remap_ids {
        return;
    }
    let Some(workspace) = obj
        .get_mut("result")
        .and_then(|result| result.get_mut("workspace"))
        .and_then(|workspace| workspace.as_object_mut())
    else {
        return;
    };
    for field in ["workspace_id", "active_tab_id"] {
        if let Some(serde_json::Value::String(local)) = workspace.get(field) {
            let prefixed = crate::app::peers::prefix_peer_id(instance_id, local);
            workspace.insert(field.to_string(), serde_json::Value::String(prefixed));
        }
    }
}

/// The detector state behind a status the peer already resolved.
///
/// `Done` and `Idle` are the same agent state seen from different sides: the
/// split is the local seen flag, which belongs to whoever is looking at the
/// pane, not to the machine running the agent. Feeding `Idle` back keeps that
/// decision here.
pub(super) fn agent_state_from_status(
    status: crate::api::schema::AgentStatus,
) -> crate::detect::AgentState {
    use crate::api::schema::AgentStatus;
    use crate::detect::AgentState;
    match status {
        AgentStatus::Idle | AgentStatus::Done => AgentState::Idle,
        AgentStatus::Working => AgentState::Working,
        AgentStatus::Blocked => AgentState::Blocked,
        AgentStatus::Unknown => AgentState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::public_pane_id_for_number;

    /// Generated with the real encoder rather than written as literals, which
    /// is the whole point: the defect this guards was two id encodings drifting
    /// apart, and a hand-written `"w1:p10"` would have agreed with the bug.
    #[test]
    fn every_pane_number_the_encoder_produces_recovers_its_workspace() {
        // 9 is the last single decimal digit, 10 the first that is not (`pA`),
        // and 33 the first that carries into a second base32 place.
        for number in [1usize, 9, 10, 33, 1024] {
            let id = public_pane_id_for_number("w1", number);
            assert_eq!(
                peer_workspace_of_pane_id(&id).as_deref(),
                Some("w1"),
                "pane {number} rendered as {id}"
            );
        }
    }

    /// The tenth pane, spelled out: this is the case that regressed, and naming
    /// it keeps the encoder's alphabet from being changed without noticing.
    #[test]
    fn the_tenth_pane_is_pa_and_still_names_its_workspace() {
        assert_eq!(public_pane_id_for_number("w1", 10), "w1:pA");
        assert_eq!(
            peer_workspace_of_pane_id("w1:pA").as_deref(),
            Some("w1"),
            "a base32 pane number is still a pane number"
        );
    }

    /// A target that names no workspace still must not have one guessed for it.
    #[test]
    fn targets_that_are_not_pane_ids_name_no_workspace() {
        for target in [
            "w1:t1",     // a tab, not a pane
            "term_18d2", // a terminal id
            "claude",    // an agent name
            "w1:p",      // the prefix alone
            "w1:pa",     // lowercase is not in the alphabet
            "w1:pI",     // nor are the letters the alphabet omits
            ":p1",       // no workspace
            "w1:x1",     // not a pane prefix
        ] {
            assert_eq!(
                peer_workspace_of_pane_id(target),
                None,
                "{target} names no workspace"
            );
        }
    }
}
