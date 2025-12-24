//! TUI for build progress tracking
//!
//! Uses indicatif MultiProgress with:
//! - One progress bar showing overall completion
//! - 3 spinner bars for longest-running active actions
//! - 10 text bars for scrolling log buffer (last 10 messages)

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, RwLock};
use tracing_subscriber::Layer;
use vx_aether_proto::ActionType;

/// Tracks a single action
#[derive(Debug, Clone)]
struct Action {
    action_type: ActionType,
    started_at: Instant,
}

impl Action {
    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// TUI state shared between the service and the display task
struct TuiState {
    /// Active actions (action_id -> Action)
    active: HashMap<u64, Action>,
    /// Total number of actions we know about
    total: usize,
    /// Number of completed actions
    completed: usize,
    /// Next action ID
    next_id: u64,
    /// Recent log messages (circular buffer, keep last 100, show last 10)
    logs: VecDeque<String>,
    /// Shutdown flag to stop the display task
    shutdown: bool,
}

impl TuiState {
    fn new() -> Self {
        Self {
            active: HashMap::new(),
            total: 0,
            completed: 0,
            next_id: 1,
            logs: VecDeque::with_capacity(100),
            shutdown: false,
        }
    }

    fn pending(&self) -> usize {
        self.total.saturating_sub(self.completed).saturating_sub(self.active.len())
    }
}

/// Handle for interacting with the TUI
pub struct TuiHandle {
    state: Arc<RwLock<TuiState>>,
    /// Receiver that signals when display task has finished cleanup
    shutdown_rx: Arc<RwLock<Option<oneshot::Receiver<()>>>>,
}

impl Clone for TuiHandle {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            shutdown_rx: self.shutdown_rx.clone(),
        }
    }
}

impl TuiHandle {
    /// Create a new TUI handle and spawn the display task
    pub fn new() -> Self {
        let state = Arc::new(RwLock::new(TuiState::new()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = Self {
            state: state.clone(),
            shutdown_rx: Arc::new(RwLock::new(Some(shutdown_rx))),
        };

        // Spawn the display task
        tokio::spawn(Self::display_task(state, shutdown_tx));

        handle
    }

    /// Set the total number of actions
    pub async fn set_total(&self, total: usize) {
        let mut state = self.state.write().await;
        state.total = total;
    }

    /// Start tracking an action, returning its ID
    pub async fn start_action(&self, action_type: ActionType) -> u64 {
        let mut state = self.state.write().await;
        let id = state.next_id;
        state.next_id += 1;

        state.active.insert(
            id,
            Action {
                action_type,
                started_at: Instant::now(),
            },
        );

        id
    }

    /// Mark an action as completed
    pub async fn complete_action(&self, id: u64) {
        let mut state = self.state.write().await;
        if state.active.remove(&id).is_some() {
            state.completed += 1;
        }
    }

    /// Add a log message to the TUI
    pub async fn log_message(&self, message: String) {
        let mut state = self.state.write().await;
        state.logs.push_back(message);
        // Keep only last 100 messages
        if state.logs.len() > 100 {
            state.logs.pop_front();
        }
    }

    /// Shutdown the TUI and wait for cleanup to complete
    pub async fn shutdown(&self) {
        // Set the shutdown flag
        {
            let mut state = self.state.write().await;
            state.shutdown = true;
        }

        // Wait for the display task to signal completion
        if let Some(rx) = self.shutdown_rx.write().await.take() {
            let _ = rx.await; // Ignore errors (channel closed is fine)
        }
    }

    /// Background task that updates the display at ~15fps
    async fn display_task(state: Arc<RwLock<TuiState>>, shutdown_tx: oneshot::Sender<()>) {
        let multi = MultiProgress::new();

        // Main progress bar
        let progress_bar = multi.add(ProgressBar::new(100));
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{bar:40.cyan/blue} {pos}/{len} [{msg}]")
                .expect("valid template")
                .progress_chars("=>-"),
        );

        // Active action spinners (3 for longest-running actions)
        let spinner_style = ProgressStyle::with_template("  {spinner} {msg}")
            .expect("valid template")
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

        let empty_style = ProgressStyle::with_template("")
            .expect("valid template");

        let action_bars: Vec<_> = (0..3)
            .map(|_| {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(empty_style.clone());
                // Don't enable steady tick - we'll tick manually only when active
                bar
            })
            .collect();

        // Log display bars (10 for scrolling buffer)
        let log_style = ProgressStyle::with_template("  {msg}")
            .expect("valid template");

        let log_bars: Vec<_> = (0..10)
            .map(|_| {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_style(log_style.clone());
                bar
            })
            .collect();

        // Update loop at ~15fps (every 67ms)
        let mut interval = tokio::time::interval(Duration::from_millis(67));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let state_snapshot = state.read().await;

            // Update progress bar
            let total = state_snapshot.total.max(1); // Avoid div by zero
            let completed = state_snapshot.completed;
            let pending = state_snapshot.pending();
            let active = state_snapshot.active.len();

            progress_bar.set_length(total as u64);
            progress_bar.set_position(completed as u64);
            progress_bar.set_message(format!(
                "completed: {}, active: {}, pending: {}",
                completed, active, pending
            ));

            // Get the 3 longest-running actions
            let mut actions: Vec<_> = state_snapshot.active.values().collect();
            actions.sort_by_key(|a| std::cmp::Reverse(a.elapsed()));

            for (i, bar) in action_bars.iter().enumerate() {
                if let Some(action) = actions.get(i) {
                    bar.set_style(spinner_style.clone());
                    let elapsed_secs = action.elapsed().as_secs_f64();
                    bar.set_message(format!(
                        "{:<40} ({:.1}s)",
                        action.action_type.display_name(),
                        elapsed_secs
                    ));
                    bar.tick(); // Manually tick to advance spinner animation
                } else {
                    // Hide inactive slots completely
                    bar.set_style(empty_style.clone());
                    bar.set_message("");
                }
            }

            // Show the last 10 log messages
            let log_count = state_snapshot.logs.len();
            let start_idx = log_count.saturating_sub(10);

            for (i, bar) in log_bars.iter().enumerate() {
                if let Some(log) = state_snapshot.logs.get(start_idx + i) {
                    bar.set_message(log.clone());
                } else {
                    bar.set_message("");
                }
            }

            // If shutdown was requested, clean up and exit
            if state_snapshot.shutdown {
                // Clear all bars in reverse order (logs, actions, progress)
                for bar in log_bars.iter().rev() {
                    bar.finish_and_clear();
                }
                for bar in action_bars.iter().rev() {
                    bar.finish_and_clear();
                }
                progress_bar.finish_and_clear();

                // Clear the MultiProgress
                drop(multi);

                // Signal that cleanup is complete
                let _ = shutdown_tx.send(());

                break;
            }
        }
    }
}

/// Custom tracing layer that sends logs to the TUI
pub struct TuiLayer {
    tui: TuiHandle,
}

impl TuiLayer {
    pub fn new(tui: TuiHandle) -> Self {
        Self { tui }
    }
}

impl<S> Layer<S> for TuiLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write;
        use tracing::field::Visit;

        // Visitor to extract the log message
        struct MessageVisitor {
            message: String,
        }

        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let _ = write!(self.message, "{:?}", value);
                } else {
                    let _ = write!(self.message, " {}={:?}", field.name(), value);
                }
            }

            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    let _ = write!(self.message, " {}={}", field.name(), value);
                }
            }
        }

        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);

        // Format with level and target
        let metadata = event.metadata();
        let level = metadata.level();
        let target = metadata.target();

        let formatted = format!("[{:<5}] {}: {}", level, target, visitor.message);

        // Send to TUI (spawn to avoid blocking)
        let tui = self.tui.clone();
        tokio::spawn(async move {
            tui.log_message(formatted).await;
        });
    }
}
