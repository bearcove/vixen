//! TUI for build progress tracking
//!
//! Displays a progress bar showing total/completed/pending actions,
//! and shows the 3 longest-running actions below the bar, plus 2 recent log lines.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::Subscriber;
use tracing_subscriber::Layer;

/// Action type being tracked
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionType {
    /// Compiling a Rust crate
    CompileRust(String),
    /// Acquiring toolchain
    AcquireToolchain,
    /// Acquiring registry crate
    AcquireRegistryCrate(String, String), // name, version
    /// Ingesting source tree to CAS
    IngestSource(String),
}

impl ActionType {
    /// Get a display name for this action
    pub fn display_name(&self) -> String {
        match self {
            ActionType::CompileRust(name) => format!("compile {}", name),
            ActionType::AcquireToolchain => "acquire toolchain".to_string(),
            ActionType::AcquireRegistryCrate(name, version) => {
                format!("acquire {}@{}", name, version)
            }
            ActionType::IngestSource(name) => format!("ingest {}", name),
        }
    }
}

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
    /// Recent log messages (circular buffer, keep last 50)
    logs: VecDeque<String>,
}

impl TuiState {
    fn new() -> Self {
        Self {
            active: HashMap::new(),
            total: 0,
            completed: 0,
            next_id: 1,
            logs: VecDeque::with_capacity(50),
        }
    }

    fn pending(&self) -> usize {
        self.total.saturating_sub(self.completed).saturating_sub(self.active.len())
    }
}

/// Handle for interacting with the TUI
#[derive(Clone)]
pub struct TuiHandle {
    state: Arc<RwLock<TuiState>>,
}

impl TuiHandle {
    /// Create a new TUI handle and spawn the display task
    pub fn new() -> Self {
        let state = Arc::new(RwLock::new(TuiState::new()));

        let handle = Self {
            state: state.clone(),
        };

        // Spawn the display task
        tokio::spawn(Self::display_task(state));

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
        // Keep only last 50 messages
        if state.logs.len() > 50 {
            state.logs.pop_front();
        }
    }

    /// Background task that updates the display at ~15fps
    async fn display_task(state: Arc<RwLock<TuiState>>) {
        let multi = MultiProgress::new();

        // Main progress bar
        let progress_bar = multi.add(ProgressBar::new(100));
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{bar:40.cyan/blue} {pos}/{len} [{msg}]")
                .expect("valid template")
                .progress_chars("=>-"),
        );

        // Action display bars (3 for longest-running actions)
        let action_bars: Vec<_> = (0..3)
            .map(|_| {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_style(
                    ProgressStyle::default_bar()
                        .template("  {msg}")
                        .expect("valid template"),
                );
                bar
            })
            .collect();

        // Log display bars (2 for recent logs)
        let log_bars: Vec<_> = (0..2)
            .map(|_| {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_style(
                    ProgressStyle::default_bar()
                        .template("  {msg}")
                        .expect("valid template"),
                );
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
                    let elapsed_secs = action.elapsed().as_secs_f64();
                    bar.set_message(format!(
                        "{:<30} ({:.1}s)",
                        action.action_type.display_name(),
                        elapsed_secs
                    ));
                } else {
                    bar.set_message("");
                }
            }

            // Show the last 2 log messages
            let log_count = state_snapshot.logs.len();
            for (i, bar) in log_bars.iter().enumerate() {
                // i=0 -> second-to-last, i=1 -> last
                let index = log_count.saturating_sub(2).saturating_add(i);
                if let Some(log) = state_snapshot.logs.get(index) {
                    bar.set_message(log.clone());
                } else {
                    bar.set_message("");
                }
            }

            // If we're done, finish and break
            if completed == total && state_snapshot.active.is_empty() && total > 0 {
                progress_bar.finish_with_message("build complete");
                for bar in &action_bars {
                    bar.finish_and_clear();
                }
                for bar in &log_bars {
                    bar.finish_and_clear();
                }
                break;
            }
        }
    }

    /// Create a tracing layer that sends log events to the TUI
    pub fn tracing_layer(&self) -> TuiLayer {
        TuiLayer {
            handle: self.clone(),
        }
    }
}

/// Tracing layer that sends log events to the TUI instead of stdout
pub struct TuiLayer {
    handle: TuiHandle,
}

impl<S> Layer<S> for TuiLayer
where
    S: Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Format the event into a string
        let mut visitor = LogVisitor::new();
        event.record(&mut visitor);

        let level = event.metadata().level();
        let target = event.metadata().target();
        let message = visitor.message;

        // Format: "LEVEL target: message"
        let formatted = format!("{:5} {}: {}", level, target, message);

        // Send to TUI (spawn task since we can't be async here)
        let handle = self.handle.clone();
        tokio::spawn(async move {
            handle.log_message(formatted).await;
        });
    }
}

/// Visitor to extract all fields from a tracing event
struct LogVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl LogVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: Vec::new(),
        }
    }

    fn format(&self) -> String {
        if self.fields.is_empty() {
            return self.message.clone();
        }

        let fields_str = self.fields
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(" ");

        if self.message.is_empty() {
            fields_str
        } else {
            format!("{} {}", self.message, fields_str)
        }
    }
}

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.push((field.name().to_string(), format!("{:?}", value)));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() != "message" {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if field.name() != "message" {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() != "message" {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }
}
