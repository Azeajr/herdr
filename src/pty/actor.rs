#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows {
    use std::io::{Read, Write};
    use std::sync::{mpsc as std_mpsc, Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use portable_pty::{MasterPty, PtySize};
    use tokio::sync::mpsc;
    use tracing::{debug, warn};

    /// Input accepted from callers before they are told the pane is saturated.
    const PTY_INPUT_QUEUE_CAPACITY: usize = 1024;

    /// Writes queued for ConPTY before producers feel it.
    ///
    /// Matched to the input queue so the two stages bound the same amount of
    /// work rather than one silently undoing the other. Bounded by items only:
    /// byte accounting would be better, and is not worth adding to code this
    /// machine can compile but not run.
    const PTY_WRITE_QUEUE_CAPACITY: usize = 1024;

    pub(crate) struct PtyReadResult {
        pub terminal_responses: Vec<Bytes>,
    }

    type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
    type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct PtyResize {
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    }

    struct PtyResizeRequest {
        resize: PtyResize,
        terminal_responses: Vec<Bytes>,
    }

    pub(crate) struct PtyIoActorConfig {
        pub pane_id: u32,
        pub master: Box<dyn MasterPty + Send>,
        pub initially_quiesced: bool,
        pub on_read: ReadCallback,
        pub on_reader_exit: Option<ReaderExitCallback>,
    }

    enum PtyIoControlCommand {
        Resize(PtyResizeRequest),
        Shutdown,
    }

    #[derive(Clone)]
    pub(crate) struct PtyIoActorHandle {
        data_tx: mpsc::Sender<Bytes>,
        control_tx: std_mpsc::Sender<PtyIoControlCommand>,
        write_tx: std_mpsc::SyncSender<Bytes>,
        response_order: Arc<Mutex<()>>,
        accepting: Arc<Mutex<bool>>,
    }

    impl PtyIoActorHandle {
        pub(crate) async fn write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::SendError<Bytes>> {
            if !*self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(mpsc::error::SendError(bytes));
            }
            self.data_tx.send(bytes).await
        }

        pub(crate) fn try_write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
            if !*self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(mpsc::error::TrySendError::Closed(bytes));
            }
            self.data_tx.try_send(bytes)
        }

        pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
            let _order = self
                .response_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(bytes) = response().filter(|bytes| !bytes.is_empty()) {
                // try_send rather than send: this is reached from whatever
                // thread is processing terminal state, and blocking it behind a
                // ConPTY that stopped accepting writes is the stall this queue
                // is bounded to avoid. A response dropped that far behind was
                // not going to be useful.
                let _ = self.write_tx.try_send(bytes);
            }
        }

        pub(crate) fn resize(
            &self,
            rows: u16,
            cols: u16,
            cell_width_px: u32,
            cell_height_px: u32,
            terminal_responses: Vec<Bytes>,
        ) {
            let _ = self
                .control_tx
                .send(PtyIoControlCommand::Resize(PtyResizeRequest {
                    resize: PtyResize {
                        rows,
                        cols,
                        cell_width_px,
                        cell_height_px,
                    },
                    terminal_responses,
                }));
        }

        pub(crate) fn shutdown(&self) {
            if let Ok(mut accepting) = self.accepting.lock() {
                *accepting = false;
            }
            let _ = self.control_tx.send(PtyIoControlCommand::Shutdown);
        }
    }

    pub(crate) struct PtyIoActor;

    impl PtyIoActor {
        pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
            let PtyIoActorConfig {
                pane_id,
                master,
                initially_quiesced,
                mut on_read,
                on_reader_exit,
            } = config;

            let mut reader = master
                .try_clone_reader()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let mut writer = master
                .take_writer()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let (data_tx, mut data_rx) = mpsc::channel::<Bytes>(PTY_INPUT_QUEUE_CAPACITY);
            let (control_tx, control_rx) = std_mpsc::channel::<PtyIoControlCommand>();
            // Bounded, because an unbounded stage here cancelled the bound
            // above it: input arrived through a 1,024-slot channel and was
            // immediately forwarded into a queue with no limit at all, so a
            // ConPTY that stopped accepting writes could accumulate without end
            // behind the writer.
            let (write_tx, write_rx) = std_mpsc::sync_channel::<Bytes>(PTY_WRITE_QUEUE_CAPACITY);
            let response_order = Arc::new(Mutex::new(()));
            let accepting = Arc::new(Mutex::new(!initially_quiesced));

            std::thread::spawn(move || {
                for bytes in write_rx {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                debug!(pane_id, "windows pty writer thread exiting");
            });

            {
                let write_tx = write_tx.clone();
                std::thread::spawn(move || {
                    while let Some(bytes) = data_rx.blocking_recv() {
                        if write_tx.send(bytes).is_err() {
                            break;
                        }
                    }
                    debug!(pane_id, "windows pty input thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let response_order = Arc::clone(&response_order);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _order = response_order
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                let result = on_read(&buf[..n]);
                                // Never blocks, unlike the input thread below:
                                // this thread is what drains the pty, so
                                // waiting here for a wedged writer would stop
                                // reading and leave the child blocked on its
                                // own output. A reply dropped against a writer
                                // that far behind was not going to arrive in
                                // time to be useful anyway.
                                let disconnected =
                                    result.terminal_responses.into_iter().any(|response| {
                                        match write_tx.try_send(response) {
                                            Ok(()) => false,
                                            Err(std_mpsc::TrySendError::Full(_)) => {
                                                warn!(
                                                    pane_id,
                                                    "windows pty writer is not keeping up; \
                                                     dropping a terminal response"
                                                );
                                                false
                                            }
                                            Err(std_mpsc::TrySendError::Disconnected(_)) => true,
                                        }
                                    });
                                if disconnected {
                                    break;
                                }
                            }
                            Err(err) => {
                                debug!(pane_id, err = %err, "windows pty reader failed");
                                break;
                            }
                        }
                    }
                    if let Some(on_reader_exit) = on_reader_exit {
                        on_reader_exit();
                    }
                    debug!(pane_id, "windows pty reader thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                std::thread::spawn(move || {
                    for command in control_rx {
                        match command {
                            PtyIoControlCommand::Resize(request) => {
                                let size = request.resize;
                                if let Err(err) = master.resize(PtySize {
                                    rows: size.rows,
                                    cols: size.cols,
                                    pixel_width: size.cell_width_px.min(u16::MAX as u32) as u16,
                                    pixel_height: size.cell_height_px.min(u16::MAX as u32) as u16,
                                }) {
                                    warn!(pane_id, err = %err, "windows pty resize failed");
                                }
                                // Same reasoning as the reader thread: this
                                // one also services shutdown, so it must not
                                // park behind a writer that is not draining.
                                let disconnected =
                                    request.terminal_responses.into_iter().any(|response| {
                                        match write_tx.try_send(response) {
                                            Ok(()) => false,
                                            Err(std_mpsc::TrySendError::Full(_)) => {
                                                warn!(
                                                    pane_id,
                                                    "windows pty writer is not keeping up; \
                                                     dropping a resize response"
                                                );
                                                false
                                            }
                                            Err(std_mpsc::TrySendError::Disconnected(_)) => true,
                                        }
                                    });
                                if disconnected {
                                    break;
                                }
                            }
                            PtyIoControlCommand::Shutdown => break,
                        }
                    }
                    debug!(pane_id, "windows pty control thread exiting");
                });
            }

            Ok(PtyIoActorHandle {
                data_tx,
                control_tx,
                write_tx,
                response_order,
                accepting,
            })
        }
    }

    #[allow(dead_code)]
    fn _assert_duration_send(_: Duration) {}
}

#[cfg(windows)]
pub(crate) use windows::*;
