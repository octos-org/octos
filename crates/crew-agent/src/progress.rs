//! Progress reporting for agent execution.
//!
//! This module provides a trait for reporting agent progress,
//! allowing CLI and other consumers to display real-time updates.

use std::time::Duration;

/// Events emitted during agent execution.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Agent started working on task.
    TaskStarted { task_id: String },

    /// Agent is thinking (calling LLM).
    Thinking { iteration: u32 },

    /// LLM responded with text.
    Response { content: String, iteration: u32 },

    /// Agent is calling a tool.
    ToolStarted { name: String, tool_id: String },

    /// Mid-execution progress from a tool (e.g., stderr line from binary plugin).
    ToolProgress {
        name: String,
        tool_id: String,
        message: String,
    },

    /// Tool execution completed.
    ToolCompleted {
        name: String,
        tool_id: String,
        success: bool,
        output_preview: String,
        duration: Duration,
    },

    /// File was modified.
    FileModified { path: String },

    /// Token usage update.
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
    },

    /// Task completed.
    TaskCompleted {
        success: bool,
        iterations: u32,
        duration: Duration,
    },

    /// Task was interrupted (Ctrl+C).
    TaskInterrupted { iterations: u32 },

    /// Hit max iterations limit.
    MaxIterationsReached { limit: u32 },

    /// Hit token budget limit.
    TokenBudgetExceeded { used: u32, limit: u32 },

    /// Hit wall-clock timeout.
    WallClockTimeoutReached { elapsed: Duration, limit: Duration },

    /// Status update during LLM call (e.g. retry progress, provider switching).
    LlmStatus { message: String, iteration: u32 },

    /// Streaming text chunk from LLM.
    StreamChunk { text: String, iteration: u32 },

    /// Streaming completed.
    StreamDone { iteration: u32 },

    /// LLM call is being retried — signals stream forwarder to clear its buffer
    /// so partial text from the failed attempt isn't concatenated with the retry.
    StreamRetry { iteration: u32 },

    /// Cost update after a response.
    CostUpdate {
        session_input_tokens: u32,
        session_output_tokens: u32,
        /// Cost of this response (None if pricing unknown).
        response_cost: Option<f64>,
        /// Cumulative session cost.
        session_cost: Option<f64>,
    },
}

/// Trait for receiving progress updates.
pub trait ProgressReporter: Send + Sync {
    /// Called when a progress event occurs.
    fn report(&self, event: ProgressEvent);
}

/// Default reporter that does nothing (silent mode).
pub struct SilentReporter;

impl ProgressReporter for SilentReporter {
    fn report(&self, _event: ProgressEvent) {}
}

/// Reporter that prints to stdout with colors.
pub struct ConsoleReporter {
    /// Whether to use colors.
    use_colors: bool,
    /// Whether to show verbose output.
    verbose: bool,
    /// Buffered stdout writer for streaming chunks.
    stdout: std::sync::Mutex<std::io::BufWriter<std::io::Stdout>>,
}

impl Default for ConsoleReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleReporter {
    /// Create a new console reporter.
    pub fn new() -> Self {
        Self {
            use_colors: true,
            verbose: false,
            stdout: std::sync::Mutex::new(std::io::BufWriter::new(std::io::stdout())),
        }
    }

    /// Enable or disable colors.
    pub fn with_colors(mut self, use_colors: bool) -> Self {
        self.use_colors = use_colors;
        self
    }

    /// Enable or disable verbose output.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    fn cyan(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[36m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    fn green(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[32m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    fn yellow(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[33m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    fn red(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[31m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    fn dim(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    fn bold(&self, s: &str) -> String {
        if self.use_colors {
            format!("\x1b[1m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
}

impl ProgressReporter for ConsoleReporter {
    fn report(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::TaskStarted { task_id } => {
                if self.verbose {
                    println!("{} Task: {}", self.dim("▶"), self.dim(&task_id));
                }
            }
            ProgressEvent::Thinking { iteration } => {
                print!(
                    "\r{} {}",
                    self.yellow("⟳"),
                    self.dim(&format!("Thinking... (iteration {})", iteration))
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            ProgressEvent::Response {
                content,
                iteration: _,
            } => {
                // Clear the "Thinking..." line
                print!("\r{}\r", " ".repeat(40));

                if !content.is_empty() {
                    // Show first few lines of response
                    let preview: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
                    let truncated = if content.lines().count() > 3 {
                        format!("{}...", preview)
                    } else {
                        preview
                    };
                    if !truncated.trim().is_empty() {
                        println!("{} {}", self.cyan("◆"), truncated);
                    }
                }
            }
            ProgressEvent::ToolProgress { name, message, .. } => {
                print!(
                    "\r{} {} {}{}",
                    self.yellow("⚙"),
                    self.dim(&name),
                    self.dim(&message),
                    " ".repeat(10),
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            ProgressEvent::ToolStarted { name, tool_id: _ } => {
                print!(
                    "\r{} {}",
                    self.yellow("⚙"),
                    self.dim(&format!("Running {}...", name))
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            ProgressEvent::ToolCompleted {
                name,
                tool_id: _,
                success,
                output_preview,
                duration,
            } => {
                // Clear the "Running..." line
                print!("\r{}\r", " ".repeat(50));

                let status = if success {
                    self.green("✓")
                } else {
                    self.red("✗")
                };

                let duration_str = if duration.as_millis() > 1000 {
                    format!("{:.1}s", duration.as_secs_f64())
                } else {
                    format!("{}ms", duration.as_millis())
                };

                println!(
                    "{} {} {}",
                    status,
                    self.bold(&name),
                    self.dim(&format!("({})", duration_str))
                );

                // Show truncated output if verbose
                if self.verbose && !output_preview.is_empty() {
                    let lines: Vec<_> = output_preview.lines().take(5).collect();
                    for line in lines {
                        println!("  {}", self.dim(line));
                    }
                    if output_preview.lines().count() > 5 {
                        println!("  {}", self.dim("..."));
                    }
                }
            }
            ProgressEvent::FileModified { path } => {
                println!("{} {} {}", self.green("📝"), self.dim("Modified:"), path);
            }
            ProgressEvent::TokenUsage {
                input_tokens,
                output_tokens,
            } => {
                if self.verbose {
                    println!(
                        "  {} {} in, {} out",
                        self.dim("Tokens:"),
                        input_tokens,
                        output_tokens
                    );
                }
            }
            ProgressEvent::TaskCompleted {
                success,
                iterations,
                duration,
            } => {
                println!();
                if success {
                    println!(
                        "{} {} iterations, {:.1}s",
                        self.green("✓ Completed"),
                        iterations,
                        duration.as_secs_f64()
                    );
                } else {
                    println!("{} after {} iterations", self.red("✗ Failed"), iterations);
                }
            }
            ProgressEvent::TaskInterrupted { iterations } => {
                println!();
                println!(
                    "{} State saved. Run 'crew resume' to continue.",
                    self.yellow(&format!("⚠ Interrupted after {} iterations.", iterations))
                );
            }
            ProgressEvent::MaxIterationsReached { limit } => {
                println!();
                println!(
                    "{} Increase with --max-iterations",
                    self.yellow(&format!("⚠ Reached max iterations limit ({}).", limit))
                );
            }
            ProgressEvent::TokenBudgetExceeded { used, limit } => {
                println!();
                println!(
                    "{} Increase with --max-tokens",
                    self.yellow(&format!(
                        "⚠ Token budget exceeded ({} used, {} limit).",
                        used, limit
                    ))
                );
            }
            ProgressEvent::WallClockTimeoutReached { limit, .. } => {
                println!();
                println!(
                    "{} Increase with --max-timeout",
                    self.yellow(&format!(
                        "⚠ Wall-clock timeout ({:.0}s limit).",
                        limit.as_secs_f64()
                    ))
                );
            }
            ProgressEvent::LlmStatus { message, .. } => {
                print!("\r{} {}", self.yellow("⟳"), self.dim(&message));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            ProgressEvent::StreamChunk { text, .. } => {
                use std::io::Write;
                if let Ok(mut buf) = self.stdout.lock() {
                    let _ = write!(buf, "{}", text);
                    // Flush only on newlines to reduce syscalls
                    if text.contains('\n') {
                        let _ = buf.flush();
                    }
                }
            }
            ProgressEvent::StreamDone { .. } => {
                use std::io::Write;
                if let Ok(mut buf) = self.stdout.lock() {
                    let _ = writeln!(buf);
                    let _ = buf.flush();
                }
            }
            ProgressEvent::StreamRetry { .. } => {
                // Console doesn't need to do anything special on retry
                // (the LlmStatus message already shows retry info).
            }
            ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                session_cost,
                ..
            } => {
                if self.verbose {
                    let cost_str = match session_cost {
                        Some(c) => format!("${:.4}", c),
                        None => "N/A".to_string(),
                    };
                    println!(
                        "  {} {} in / {} out | Cost: {}",
                        self.dim("Tokens:"),
                        session_input_tokens,
                        session_output_tokens,
                        cost_str,
                    );
                }
            }
        }
    }
}
