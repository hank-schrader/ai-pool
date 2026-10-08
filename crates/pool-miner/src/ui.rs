//! Terminal progress and prompts. Without a terminal, progress is logged.

use std::{
    io::IsTerminal,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use indicatif::{ProgressBar, ProgressStyle};
use pool_runtime::Stage;

pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

pub fn gib(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// Progress for one artifact; pass `&|s, d, t| reporter.update(s, d, t)`.
pub struct Reporter {
    label: String,
    bar: Option<ProgressBar>,
    stage: Mutex<Option<Stage>>,
    logged_tenth: AtomicU64,
}

impl Reporter {
    pub fn new(label: impl Into<String>) -> Self {
        let bar = std::io::stderr().is_terminal().then(|| {
            let bar = ProgressBar::hidden();
            bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
            bar
        });
        Self { label: label.into(), bar, stage: Mutex::new(None), logged_tenth: AtomicU64::new(u64::MAX) }
    }

    pub fn update(&self, stage: Stage, done: u64, total: u64) {
        let verb = match stage {
            Stage::Downloading => "downloading",
            Stage::Verifying => "verifying",
            Stage::Extracting => "extracting",
        };
        let mut current = self.stage.lock().unwrap();
        if *current != Some(stage) {
            *current = Some(stage);
            self.logged_tenth.store(u64::MAX, Ordering::Relaxed);
            if let Some(bar) = &self.bar {
                let template = if stage == Stage::Extracting {
                    "{msg} [{bar:30}] {pos}/{len} archives"
                } else {
                    "{msg} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}"
                };
                bar.set_style(ProgressStyle::with_template(template).unwrap().progress_chars("=> "));
                bar.set_length(total);
                bar.set_message(format!("{verb} {}", self.label));
            }
        }
        match &self.bar {
            Some(bar) => {
                bar.set_position(done);
                if done >= total {
                    bar.finish();
                }
            }
            None => {
                let tenth = (done * 10).checked_div(total).unwrap_or(10);
                if self.logged_tenth.swap(tenth, Ordering::Relaxed) != tenth {
                    tracing::info!("{verb} {}: {}%", self.label, tenth * 10);
                }
            }
        }
    }
}
