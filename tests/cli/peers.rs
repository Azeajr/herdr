use super::harness::*;

/// Drives the deferred peer split end to end against two real servers.
///
/// Everything below this is unit-level: the routing gate answers with a fake
/// control endpoint, and the split response is parsed from a fixture. Neither
/// covers the part that only exists across a process boundary — alpha sending
/// `pane.split` to beta, beta creating a pane, alpha connecting a second view
/// onto it, and the answer coming back through the deferred path — nor the
/// claim that closing that view closes the pane it caused.
#[test]
fn splitting_a_peer_backed_pane_creates_and_closes_the_pane_on_the_peer() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(10),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(10),
    );

    // beta owns the terminal; alpha owns nothing but a view onto it.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--label",
            "remote-ws",
            "--focus",
        ],
    );

    let beta_socket = named_session_socket(&config_home, "beta");
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "peer",
            "add",
            "beta",
            "--socket",
            beta_socket.to_str().unwrap(),
            "--json",
        ],
    );

    // The peer thread connects and enumerates on its own schedule, so the
    // workspace to open is whatever it has reported by the time it is asked.
    let peer_list = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "peer", "list", "--json"],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let peers = peer_list();
            peers["result"]["peers"][0]["connection"] == "connected"
                && peers["result"]["peers"][0]["workspaces"]
                    .as_array()
                    .is_some_and(|workspaces| !workspaces.is_empty())
        }),
        "alpha never saw beta's workspace: {}",
        peer_list()
    );

    let peers = peer_list();
    let target = peers["result"]["peers"][0]["workspaces"][0]["workspace_id"]
        .as_str()
        .expect("a reported workspace carries a namespaced id")
        .to_string();
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "peer", "open", &target, "--focus"],
    );

    let alpha_panes = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "pane", "list"],
        )["result"]["panes"]
            .as_array()
            .expect("pane.list returns panes")
            .clone()
    };
    let beta_pane_count = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "beta", "pane", "list"],
        )["result"]["panes"]
            .as_array()
            .expect("pane.list returns panes")
            .len()
    };

    let opened = alpha_panes();
    assert_eq!(opened.len(), 1, "alpha should hold one view: {opened:?}");
    let view_pane_id = opened[0]["pane_id"]
        .as_str()
        .expect("a pane has an id")
        .to_string();
    assert_eq!(
        opened[0]["peer"], "beta",
        "the view must report the peer behind it"
    );
    assert_eq!(beta_pane_count(), 1);

    // The view holds no terminal state, so every fact about the pane behind it
    // has to arrive from beta. Answered locally these are null, which reads as
    // "an unlabeled pane" rather than "the shell is on another machine".
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let panes = alpha_panes();
            panes[0]["cwd"].is_string() && panes[0]["terminal_title"].is_string()
        }),
        "beta never labeled the view onto its pane: {:?}",
        alpha_panes()
    );

    // The split has to happen on beta: a local shell beside a remote view would
    // be a different machine in the same tab.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "pane",
            "split",
            &view_pane_id,
            "--direction",
            "right",
        ],
    );

    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            alpha_panes().len() == 2 && beta_pane_count() == 2
        }),
        "the split never landed on both sides: alpha={:?} beta={}",
        alpha_panes(),
        beta_pane_count()
    );

    let after_split = alpha_panes();
    for pane in &after_split {
        assert_eq!(
            pane["peer"], "beta",
            "both panes are views onto beta: {pane:?}"
        );
    }
    let split_pane_id = after_split
        .iter()
        .map(|pane| pane["pane_id"].as_str().expect("a pane has an id"))
        .find(|pane_id| *pane_id != view_pane_id)
        .expect("the split produced a second pane")
        .to_string();

    // alpha asked beta for that pane, so closing the view it was made for takes
    // it with them. The pane alpha only ever looked at stays running.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "pane", "close", &split_pane_id],
    );
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            alpha_panes().len() == 1 && beta_pane_count() == 1
        }),
        "closing the split view left panes behind: alpha={:?} beta={}",
        alpha_panes(),
        beta_pane_count()
    );

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "alpha"]);
    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

/// Renaming a tab inside a peer view renames the tab the peer owns.
///
/// `tab.create` in a peer view already creates the tab on the peer, so its name
/// belongs to the peer too — before this, alpha renamed its own copy and beta
/// kept calling the tab `1` forever. The forward is fire-and-forget behind the
/// local rename, so the assertion has to wait for it rather than read the
/// response.
#[test]
fn renaming_a_tab_in_a_peer_view_renames_it_on_the_peer() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(10),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(10),
    );

    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--label",
            "remote-ws",
            "--focus",
        ],
    );

    let beta_socket = named_session_socket(&config_home, "beta");
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "peer",
            "add",
            "beta",
            "--socket",
            beta_socket.to_str().unwrap(),
            "--json",
        ],
    );

    let peer_list = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "peer", "list", "--json"],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let peers = peer_list();
            peers["result"]["peers"][0]["connection"] == "connected"
                && peers["result"]["peers"][0]["workspaces"]
                    .as_array()
                    .is_some_and(|workspaces| !workspaces.is_empty())
        }),
        "alpha never saw beta's workspace: {}",
        peer_list()
    );

    let target = peer_list()["result"]["peers"][0]["workspaces"][0]["workspace_id"]
        .as_str()
        .expect("a reported workspace carries a namespaced id")
        .to_string();
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "peer", "open", &target, "--focus"],
    );

    let alpha_tabs = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "tab", "list"],
        )["result"]["tabs"]
            .as_array()
            .expect("tab.list returns tabs")
            .clone()
    };
    let beta_tab_labels = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "beta", "tab", "list"],
        )["result"]["tabs"]
            .as_array()
            .expect("tab.list returns tabs")
            .iter()
            .map(|tab| tab["label"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
    };

    let view_tab_id = alpha_tabs()[0]["tab_id"]
        .as_str()
        .expect("a tab has an id")
        .to_string();
    assert_eq!(beta_tab_labels(), vec!["1".to_string()]);

    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "tab",
            "rename",
            &view_tab_id,
            "named-from-alpha",
        ],
    );

    // Local first: the label you typed changes here whether or not the peer is
    // reachable.
    assert_eq!(
        alpha_tabs()[0]["label"],
        "named-from-alpha",
        "the local view kept its old label"
    );
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            beta_tab_labels() == vec!["named-from-alpha".to_string()]
        }),
        "beta's tab never took the name: {:?}",
        beta_tab_labels()
    );

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "alpha"]);
    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

/// Drives `agent.explain` on a peer-backed pane against two real servers.
///
/// The unit tests cover the routing gate and the stamp, neither of which proves
/// the part that only exists across a process boundary: that beta accepts the
/// peer-local id alpha sends, replays its own rules against its own screen, and
/// that the evidence coming back is beta's rather than the all-unmatched dump
/// alpha would produce from a screen it does not have.
#[test]
fn explaining_a_peer_backed_agent_returns_the_peers_own_evidence() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(10),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(10),
    );

    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--label",
            "agent-ws",
            "--focus",
        ],
    );

    let beta_socket = named_session_socket(&config_home, "beta");
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "peer",
            "add",
            "beta",
            "--socket",
            beta_socket.to_str().unwrap(),
            "--json",
        ],
    );

    let peer_list = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "peer", "list", "--json"],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let peers = peer_list();
            peers["result"]["peers"][0]["connection"] == "connected"
                && peers["result"]["peers"][0]["workspaces"]
                    .as_array()
                    .is_some_and(|workspaces| !workspaces.is_empty())
        }),
        "alpha never saw beta's workspace: {}",
        peer_list()
    );

    let peers = peer_list();
    let target = peers["result"]["peers"][0]["workspaces"][0]["workspace_id"]
        .as_str()
        .expect("a reported workspace carries a namespaced id")
        .to_string();
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "peer", "open", &target, "--focus"],
    );

    let beta_panes = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "beta", "pane", "list"],
    )["result"]["panes"]
        .as_array()
        .expect("pane.list returns panes")
        .clone();
    let beta_pane_id = beta_panes[0]["pane_id"]
        .as_str()
        .expect("a pane has an id")
        .to_string();

    // Detection runs where the screen is, so beta is the side that learns of an
    // agent. The source is deliberately not `herdr:claude`: that pairing is a
    // reserved native state source, which records a session ref and never takes
    // authority, so the pane would stay agentless. It is also not a
    // full-lifecycle source, so beta keeps replaying its screen rules instead of
    // deferring to the report — which is the evidence this test is after.
    // Reporting answers with an exit code and no body, so it is checked rather
    // than parsed.
    let reported = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "pane",
            "report-agent",
            &beta_pane_id,
            "--source",
            "herdr:test",
            "--agent",
            "claude",
            "--state",
            "working",
        ],
    );
    assert!(
        reported.status.success(),
        "beta refused the agent report: {}",
        String::from_utf8_lossy(&reported.stderr)
    );

    let alpha_panes = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "pane", "list"],
        )["result"]["panes"]
            .as_array()
            .expect("pane.list returns panes")
            .clone()
    };
    // The report has to travel back before the pane is an agent target here.
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            alpha_panes()[0]["agent"] == "claude"
        }),
        "beta's agent never reached alpha: {:?}",
        alpha_panes()
    );
    let view_pane_id = alpha_panes()[0]["pane_id"]
        .as_str()
        .expect("a pane has an id")
        .to_string();

    // Beta is the side that replays screen rules, so the region_bytes assertion
    // below needs beta's shell to have printed something first. The agent report
    // travelling back to alpha says nothing about that, so without this wait
    // explain races the shell's first output and reads an empty screen. `visible`
    // rather than `recent`: beta is headless with no attached client.
    let beta_screen = || {
        run_named_cli(
            &config_home,
            &runtime_dir,
            &[
                "--session",
                "beta",
                "pane",
                "read",
                &beta_pane_id,
                "--source",
                "visible",
            ],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let read = beta_screen();
            read.status.success() && !String::from_utf8_lossy(&read.stdout).trim().is_empty()
        }),
        "beta's pane never printed anything for the rules to run against: {}",
        String::from_utf8_lossy(&beta_screen().stderr)
    );

    let explain = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "agent",
            "explain",
            &view_pane_id,
            "--json",
        ],
    );

    // Whose rules these are, which is what stops someone editing the manifest on
    // the wrong machine.
    assert_eq!(explain["peer"], "beta", "explain must name the peer");
    assert_eq!(
        explain["peer_pane_id"], beta_pane_id,
        "explain must name the pane on the peer"
    );
    assert_eq!(explain["agent"], "claude");

    // The discriminator against the old answer: alpha holds no screen for this
    // pane, so rules replayed here see zero bytes in every region. Beta's shell
    // has a real one.
    let evaluated = explain["evaluated_rules"]
        .as_array()
        .expect("explain lists the rules it evaluated");
    assert!(
        !evaluated.is_empty(),
        "the peer replayed no rules at all: {explain}"
    );
    assert!(
        evaluated.iter().any(|rule| rule["evidence"]["region_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)),
        "the rules ran against an empty screen, so they did not run on the peer: {explain}"
    );

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "alpha"]);
    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

/// Drives `tab.create` inside a peer-backed workspace against two real servers.
///
/// The unit test covers the routing gate, which only proves the request is
/// intercepted. It cannot show the part that matters: that the tab is created on
/// beta and that alpha's new tab is a view onto it rather than a local shell
/// spawned inside what the user sees as a remote workspace.
#[test]
fn creating_a_tab_in_a_peer_backed_workspace_creates_it_on_the_peer() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(10),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(10),
    );

    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--label",
            "remote-ws",
            "--focus",
        ],
    );

    let beta_socket = named_session_socket(&config_home, "beta");
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "peer",
            "add",
            "beta",
            "--socket",
            beta_socket.to_str().unwrap(),
            "--json",
        ],
    );

    let peer_list = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "peer", "list", "--json"],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let peers = peer_list();
            peers["result"]["peers"][0]["connection"] == "connected"
                && peers["result"]["peers"][0]["workspaces"]
                    .as_array()
                    .is_some_and(|workspaces| !workspaces.is_empty())
        }),
        "alpha never saw beta's workspace: {}",
        peer_list()
    );

    let peers = peer_list();
    let target = peers["result"]["peers"][0]["workspaces"][0]["workspace_id"]
        .as_str()
        .expect("a reported workspace carries a namespaced id")
        .to_string();
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "peer", "open", &target, "--focus"],
    );

    let alpha_tabs = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "tab", "list"],
        )["result"]["tabs"]
            .as_array()
            .expect("tab.list returns tabs")
            .clone()
    };
    let beta_tab_count = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "beta", "tab", "list"],
        )["result"]["tabs"]
            .as_array()
            .expect("tab.list returns tabs")
            .len()
    };
    let alpha_panes = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "pane", "list"],
        )["result"]["panes"]
            .as_array()
            .expect("pane.list returns panes")
            .clone()
    };

    assert_eq!(alpha_tabs().len(), 1);
    assert_eq!(beta_tab_count(), 1);

    // The tab has to be created on beta. Created locally it would be a shell on
    // this machine inside a workspace the user opened to work on another one.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "tab", "create"],
    );

    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            alpha_tabs().len() == 2 && beta_tab_count() == 2
        }),
        "the tab never landed on both sides: alpha={:?} beta={}",
        alpha_tabs(),
        beta_tab_count()
    );

    // Both of alpha's panes are views; neither is a local pty.
    let panes = alpha_panes();
    assert_eq!(panes.len(), 2, "alpha should hold two views: {panes:?}");
    for pane in &panes {
        assert_eq!(
            pane["peer"], "beta",
            "every pane in a peer-backed workspace is a view onto beta: {pane:?}"
        );
    }

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "alpha"]);
    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

/// Drives the routed worktree round trip end to end against two real servers.
///
/// The unit tests prove the gate answers correctly and that a forwarded list is
/// restated in local ids. Neither covers what only exists across a process
/// boundary: beta running `git worktree add` in its own repo, alpha connecting a
/// view onto the workspace that produced, and `worktree remove` deleting the
/// checkout there and closing the view here.
///
/// The failure this guards is quiet. Run locally, `git worktree` against beta's
/// cwd usually fails — but both servers here share a filesystem, which is
/// exactly the case where the local path would have succeeded against the wrong
/// machine's repo and looked correct.
#[test]
fn worktree_actions_in_a_peer_view_run_on_the_peer() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let repo = base.join("repo");
    let checkout = base.join("checkout");

    init_test_repo(&repo);

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(10),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(10),
    );

    // beta owns the repo; alpha owns nothing but a view onto a workspace in it.
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--cwd",
            repo.to_str().unwrap(),
            "--label",
            "repo-ws",
            "--focus",
        ],
    );

    let beta_socket = named_session_socket(&config_home, "beta");
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "peer",
            "add",
            "beta",
            "--socket",
            beta_socket.to_str().unwrap(),
            "--json",
        ],
    );

    let peer_list = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "peer", "list", "--json"],
        )
    };
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let peers = peer_list();
            peers["result"]["peers"][0]["connection"] == "connected"
                && peers["result"]["peers"][0]["workspaces"]
                    .as_array()
                    .is_some_and(|workspaces| !workspaces.is_empty())
        }),
        "alpha never saw beta's workspace: {}",
        peer_list()
    );

    let peers = peer_list();
    let target = peers["result"]["peers"][0]["workspaces"][0]["workspace_id"]
        .as_str()
        .expect("a reported workspace carries a namespaced id")
        .to_string();
    let opened = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "peer", "open", &target, "--focus"],
    );
    let view_id = opened["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("the opened view has a local id")
        .to_string();

    let beta_workspace_count = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "beta", "workspace", "list"],
        )["result"]["workspaces"]
            .as_array()
            .expect("workspace.list returns workspaces")
            .len()
    };
    let alpha_workspace_ids = || {
        run_named_cli_json(
            &config_home,
            &runtime_dir,
            &["--session", "alpha", "workspace", "list"],
        )["result"]["workspaces"]
            .as_array()
            .expect("workspace.list returns workspaces")
            .iter()
            .filter_map(|ws| ws["workspace_id"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
    };

    // The list is beta's repo, answered by beta. Its source names the view here,
    // because that is the id alpha's caller can act on.
    let listed = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "worktree",
            "list",
            "--workspace",
            &view_id,
        ],
    );
    assert_eq!(
        listed["result"]["source"]["repo_root"],
        serde_json::Value::String(repo.display().to_string()),
        "the list must be beta's repo: {listed}"
    );
    assert_eq!(
        listed["result"]["source"]["source_workspace_id"],
        serde_json::Value::String(view_id.clone()),
        "the source must be named by the local view: {listed}"
    );

    // An explicit path so the checkout lands in the test's own directory rather
    // than in whatever worktree directory the machine running CI has. Where an
    // absent path goes is beta's choice and is covered by the scenario suite.
    let created = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "worktree",
            "create",
            "--workspace",
            &view_id,
            "--branch",
            "worktree/routed",
            "--path",
            checkout.to_str().unwrap(),
        ],
    );
    assert_eq!(
        created["result"]["type"], "worktree_created",
        "create should have been answered by beta: {created}"
    );
    let created_view = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("the created workspace has a local id")
        .to_string();
    // The half that matters: the checkout exists, beta holds the workspace for
    // it, and what came back here is a view rather than a local pty.
    assert!(
        checkout.join("README.md").is_file(),
        "beta should have created the checkout at {}",
        checkout.display()
    );
    assert_eq!(created["result"]["root_pane"]["peer"], "beta");
    assert_eq!(
        created["result"]["worktree"]["open_workspace_id"],
        serde_json::Value::String(created_view.clone())
    );
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            beta_workspace_count() == 2
        }),
        "beta never gained the worktree workspace"
    );

    // Removing deletes it on beta and closes the view that showed it.
    let removed = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "worktree",
            "remove",
            "--workspace",
            &created_view,
        ],
    );
    assert_eq!(
        removed["result"]["workspace_id"],
        serde_json::Value::String(created_view.clone()),
        "the removal must be reported against the local view: {removed}"
    );
    assert!(
        !checkout.exists(),
        "beta should have removed the checkout at {}",
        checkout.display()
    );
    assert!(
        !alpha_workspace_ids().contains(&created_view),
        "the view onto the removed checkout should be closed: {:?}",
        alpha_workspace_ids()
    );
    assert!(
        wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
            beta_workspace_count() == 1
        }),
        "beta never closed the worktree workspace"
    );

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "alpha"]);
    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

/// A repo with one commit, so `git worktree add` has something to check out.
fn init_test_repo(repo: &std::path::Path) {
    std::fs::create_dir_all(repo).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            repo.display()
        );
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.email", "herdr@example.invalid"]);
    git(&["config", "user.name", "Herdr Test"]);
    std::fs::write(repo.join("README.md"), "peer worktree test\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "--quiet", "-m", "initial"]);
}
