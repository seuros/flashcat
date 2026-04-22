use ratatui::crossterm::{
    cursor,
    style::{Color, Print, SetForegroundColor, ResetColor},
    terminal,
    QueueableCommand,
};
use std::io::{stdout, Write};
use std::time::Instant;

pub struct Progress {
    label: String,
    total: u64,
    current: u64,
    started: Instant,
}

impl Progress {
    pub fn new(label: &str, total: u64) -> Self {
        Self {
            label: label.to_string(),
            total,
            current: 0,
            started: Instant::now(),
        }
    }

    pub fn inc(&mut self, n: u64) {
        self.current += n;
        if let Err(e) = self.render() {
            tracing::warn!("progress render error: {e}");
        }
    }

    pub fn finish(&self) {
        println!();
    }

    fn render(&self) -> std::io::Result<()> {
        let width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
        let ratio = if self.total == 0 { 0.0 } else { self.current as f64 / self.total as f64 };
        let pct = (ratio * 100.0) as u8;

        let elapsed = self.started.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 { self.current as f64 / elapsed } else { 0.0 };
        let speed_str = format_speed(speed);

        let right = format!(" {}/{} {}", fmt_bytes(self.current), fmt_bytes(self.total), speed_str);
        let label_part = format!(" {} {:3}% ", self.label, pct);
        let bar_width = width.saturating_sub(label_part.len() + right.len() + 2);

        let filled = ((ratio * bar_width as f64) as usize).min(bar_width);
        let bar: String = "=".repeat(filled) + &" ".repeat(bar_width - filled);

        let mut out = stdout();
        out.queue(cursor::MoveToColumn(0))?;
        out.queue(SetForegroundColor(Color::Cyan))?;
        out.queue(Print(format!("{label_part}[{bar}]{right}")))?;
        out.queue(ResetColor)?;
        out.flush()
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.1}MB", b as f64 / (1024.0 * 1024.0))
    } else if b >= 1024 {
        format!("{:.1}KB", b as f64 / 1024.0)
    } else {
        format!("{b}B")
    }
}

fn format_speed(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.1}MB/s", bps / (1024.0 * 1024.0))
    } else if bps >= 1024.0 {
        format!("{:.1}KB/s", bps / 1024.0)
    } else {
        format!("{bps:.0}B/s")
    }
}
