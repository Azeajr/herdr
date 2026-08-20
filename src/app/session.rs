use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: Box<crate::persist::SessionSnapshot>,
        /// Named on the loop, read on the save thread. See
        /// [`crate::persist::SessionHistoryCapture`].
        history: Option<crate::persist::SessionHistoryCapture>,
    },
}

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if !self.no_session {
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    fn reap_finished_session_save(&mut self) {
        if self
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.session_save_thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn capture_session_save_job(&self) -> SessionSaveJob {
        // Peers are session state too: clearing on an empty workspace list would
        // silently drop every configured peer.
        if self.state.workspaces.is_empty() && self.state.peers.iter().next().is_none() {
            SessionSaveJob::Clear
        } else {
            let snapshot = Box::new(crate::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
                self.state.sidebar_width,
                self.state.sidebar_section_split,
                self.state.collapsed_space_keys.clone(),
                self.state.hidden_peers.clone(),
                crate::persist::peer_snapshots(&self.state.peers),
            ));
            // Only the pane positions and a handle each, which is what has to
            // agree with the structural snapshot above. Reading the history
            // itself took 5-10 ms per populated pane when it happened here, on
            // the loop, against a 16.7 ms frame; it now happens on the save
            // thread.
            let history = self.persist_pane_history.then(|| {
                let started = crate::render_prof::timer();
                let history = crate::persist::capture_history(
                    &self.state.workspaces,
                    &self.terminal_runtimes,
                );
                crate::render_prof::histogram_since("persist.capture_history", started);
                history
            });
            SessionSaveJob::Save { snapshot, history }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if self.no_session {
            self.session_save_deadline = None;
            return;
        }

        self.reap_finished_session_save();
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(Instant::now() + Duration::from_millis(250));
            return;
        }

        let job = self.capture_session_save_job();
        self.session_save_deadline = None;
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                run_session_save_job(self.capture_session_save_job());
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_save_thread.take() {
            let _ = thread.join();
        }

        if self.no_session {
            self.session_save_deadline = None;
            return;
        }

        run_session_save_job(self.capture_session_save_job());
        self.session_save_deadline = None;
    }
}

fn run_session_save_job(job: SessionSaveJob) {
    match job {
        SessionSaveJob::Clear => crate::persist::clear(),
        SessionSaveJob::Save { snapshot, history } => {
            // Where the retained scrollback is actually materialized.
            let started = crate::render_prof::timer();
            let history = history.map(crate::persist::SessionHistoryCapture::resolve);
            crate::render_prof::histogram_since("persist.resolve_history", started);
            crate::persist::save(&snapshot, history.as_ref());
        }
    }
}
