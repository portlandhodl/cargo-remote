//! Live transfer status: a throttled single-line display on the terminal
//! (bytes, percentage, throughput, ETA), plus a closing summary line.
//!
//! Everything is written to stderr. The live line is drawn only when stderr
//! is a terminal and the user did not pass `--no-sync-status`; without a
//! terminal only the final summary is printed, and only in verbose mode
//! (matching the pre-progress behavior).

use std::io::Write as _;
use std::time::{Duration, Instant};

use terminal_size::terminal_size_of;

use crate::runner::Runner;
use crate::transfer::human;

/// Minimum interval between redraws of the live status line.
const REDRAW: Duration = Duration::from_millis(100);

/// Live status for one transfer phase (sync, agent upload, copy-back).
///
/// Two byte counters are tracked separately: `done` counts *planned* bytes
/// processed (drives the percentage/bar against `total`), while `wire`
/// counts bytes that actually crossed the wire (drives the rate display).
/// With delta syncs the wire amount is typically much smaller.
pub struct Progress {
    label: String,
    /// Draw the live line (stderr is a TTY and status not disabled).
    live: bool,
    /// Print the final summary when not live.
    verbose: bool,
    /// Terminal width in cells, captured at creation (only when live).
    cols: Option<usize>,
    /// Planned total bytes, when known (percentage/bar).
    total: Option<u64>,
    /// Planned bytes processed so far.
    done: u64,
    /// Bytes that actually crossed the wire.
    wire: u64,
    files_done: u64,
    files_total: Option<u64>,
    /// Transient phase text ("scanning local files..."); replaces the
    /// byte display while set.
    status: Option<String>,
    start: Instant,
    last_draw: Instant,
    /// Display width of the last line drawn (for erasure).
    last_len: usize,
    finished: bool,
}

impl Progress {
    pub fn new(label: &str, runner: &Runner) -> Self {
        let cols = if runner.sync_status {
            terminal_size_of(std::io::stderr()).map(|(w, _)| w.0 as usize)
        } else {
            None
        };
        Progress {
            label: label.to_string(),
            live: runner.sync_status,
            verbose: runner.verbose,
            cols,
            total: None,
            done: 0,
            wire: 0,
            files_done: 0,
            files_total: None,
            status: None,
            start: Instant::now(),
            last_draw: Instant::now() - REDRAW,
            last_len: 0,
            finished: false,
        }
    }

    pub fn set_totals(&mut self, bytes: Option<u64>, files: Option<u64>) {
        self.total = bytes;
        self.files_total = files;
    }

    /// Bytes that actually crossed the wire so far.
    pub fn wire(&self) -> u64 {
        self.wire
    }

    /// Show a transient phase message (drawn immediately).
    pub fn status(&mut self, msg: &str) {
        self.status = Some(msg.to_string());
        self.draw(true);
    }

    /// Back to byte counting after a transient phase.
    pub fn clear_status(&mut self) {
        self.status = None;
    }

    /// Record `planned` processed bytes and `wire` transferred bytes.
    pub fn advance(&mut self, planned: u64, wire: u64) {
        self.done += planned;
        self.wire += wire;
        self.draw(false);
    }

    /// Record transferred bytes without planned-total progress.
    pub fn add_wire(&mut self, n: u64) {
        self.advance(0, n);
    }

    pub fn inc_file(&mut self) {
        self.files_done += 1;
        self.draw(false);
    }

    /// Overall wire throughput since creation, bytes/second.
    fn rate(&self) -> f64 {
        let secs = self.start.elapsed().as_secs_f64();
        if secs < 0.05 { 0.0 } else { self.wire as f64 / secs }
    }

    /// Close out: erase the live line and print the final summary as
    /// `cargo-remote: <label>: <summary> in <elapsed> (<rate>/s)`.
    /// The timing suffix is added when anything crossed the wire.
    pub fn finish(mut self, summary: &str) {
        self.finished = true;
        let mut line = summary.to_string();
        if self.wire > 0 {
            let secs = self.start.elapsed().as_secs_f64();
            let rate = self.rate();
            line.push_str(&format!(" in {secs:.1}s"));
            if rate > 0.0 {
                line.push_str(&format!(" ({}/s)", human(rate as u64)));
            }
        }
        if self.live {
            self.clear_line();
            eprintln!("cargo-remote: {}: {line}", self.label);
        } else if self.verbose {
            eprintln!("cargo-remote: {}: {line}", self.label);
        }
    }

    fn render(&self) -> String {
        let mut s = format!("cargo-remote: {}: ", self.label);
        if let Some(st) = &self.status {
            s.push_str(st);
            return self.fit(s);
        }
        let total = self.total.filter(|&t| t > 0);
        match total {
            Some(t) => {
                let done = self.done.min(t);
                s.push_str(&format!(
                    "{}/{} ({:>2}%)",
                    human(done),
                    human(t),
                    done * 100 / t
                ));
            }
            None => s.push_str(&human(self.wire)),
        }
        s.push_str(&format!(", {}/s", human(self.rate() as u64)));
        if let Some(t) = total {
            let rate = self.rate();
            if rate > 0.0 && self.done < t {
                let eta = ((t - self.done) as f64 / rate) as u64;
                s.push_str(&format!(", eta {eta}s"));
            }
        }
        if let Some(ft) = self.files_total {
            s.push_str(&format!(", files {}/{}", self.files_done.min(ft), ft));
        }
        if let (Some(t), Some(cols)) = (total, self.cols) {
            // Append a bar when the terminal has room for at least 10 cells.
            let used = s.chars().count() + 3; // " [" + "]"
            if cols > used + 8 {
                let width = (cols - used).min(30);
                let filled = (self.done.min(t) as u128 * width as u128 / t as u128) as usize;
                s.push(' ');
                s.push('[');
                if filled >= width {
                    s.push_str(&"=".repeat(width));
                } else {
                    s.push_str(&"=".repeat(filled));
                    s.push('>');
                    s.push_str(&" ".repeat(width - filled - 1));
                }
                s.push(']');
            }
        }
        self.fit(s)
    }

    /// Truncate to the terminal width (minus one cell) when known.
    fn fit(&self, s: String) -> String {
        if let Some(cols) = self.cols {
            let max = cols.saturating_sub(1);
            if s.chars().count() > max {
                return s.chars().take(max).collect();
            }
        }
        s
    }

    fn draw(&mut self, force: bool) {
        if !self.live || (!force && self.last_draw.elapsed() < REDRAW) {
            return;
        }
        self.last_draw = Instant::now();
        let line = self.render();
        let len = line.chars().count();
        let pad = self.last_len.saturating_sub(len);
        eprint!("\r{line}{:pad$}", "", pad = pad);
        let _ = std::io::stderr().flush();
        self.last_len = len;
    }

    fn clear_line(&mut self) {
        if self.last_len > 0 {
            eprint!("\r{:width$}\r", "", width = self.last_len);
            let _ = std::io::stderr().flush();
            self.last_len = 0;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // On early return (error paths) don't leave a half-drawn line.
        if self.live && !self.finished {
            self.clear_line();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(live: bool, verbose: bool) -> Runner {
        Runner {
            dry_run: false,
            verbose,
            sync_status: live,
        }
    }

    /// Progress with a fake 100-cell terminal, for deterministic rendering.
    fn progress(label: &str) -> Progress {
        let mut p = Progress::new(label, &runner(false, false));
        p.cols = Some(100);
        p
    }

    #[test]
    fn renders_bytes_percentage_and_files() {
        let mut p = progress("sync");
        p.set_totals(Some(4 << 20), Some(4));
        p.advance(1 << 20, 100_000);
        p.inc_file();
        let s = p.render();
        assert!(s.contains("1.0 MiB/4.0 MiB (25%)"), "{s}");
        assert!(s.contains(", files 1/4"), "{s}");
        assert!(s.contains("/s"), "{s}");
    }

    #[test]
    fn renders_unknown_total_as_plain_wire_bytes() {
        let mut p = progress("sync (tar)");
        p.add_wire(3 << 20);
        let s = p.render();
        assert!(s.contains("sync (tar): 3.0 MiB"), "{s}");
        assert!(!s.contains('%'), "{s}");
    }

    #[test]
    fn status_text_replaces_byte_display() {
        let mut p = progress("sync");
        p.set_totals(Some(100), Some(1));
        p.status("computing deltas: 2 of 7");
        let s = p.render();
        assert!(s.contains("computing deltas: 2 of 7"), "{s}");
        assert!(!s.contains("0 B/100 B"), "no byte counters: {s}");
        p.clear_status();
        assert!(p.render().contains("0 B/100 B"));
    }

    #[test]
    fn bar_is_appended_with_totals_and_room() {
        let mut p = progress("sync");
        p.set_totals(Some(10), None);
        p.advance(5, 5);
        let s = p.render();
        assert!(s.contains('[') && s.contains(']'), "{s}");
        // No bar without a known total.
        let mut q = progress("sync (tar)");
        q.add_wire(5);
        assert!(!q.render().contains('['));
        // No bar when the terminal is too narrow.
        let mut r = progress("sync");
        r.cols = Some(40);
        r.set_totals(Some(10), None);
        r.advance(5, 5);
        assert!(!r.render().contains('['), "{}", r.render());
    }

    #[test]
    fn long_lines_are_truncated_to_terminal() {
        let mut p = progress("sync");
        p.cols = Some(30);
        p.status("a very long phase description that will not fit");
        let s = p.render();
        assert_eq!(s.chars().count(), 29, "{s}");
    }

    #[test]
    fn eta_shown_while_incomplete() {
        let mut p = progress("sync");
        p.set_totals(Some(1 << 30), None);
        p.done = 1;
        p.wire = 1 << 20;
        // Backdate the start so the average rate is meaningful.
        p.start = Instant::now() - Duration::from_secs(10);
        let s = p.render();
        assert!(s.contains(", eta "), "{s}");
        // Complete: no eta.
        p.done = 1 << 30;
        assert!(!p.render().contains(", eta "));
    }

    #[test]
    fn non_live_non_verbose_stays_quiet() {
        let p = Progress::new("sync", &runner(false, false));
        assert!(!p.live && !p.verbose);
        p.finish("nothing should be printed");
    }
}
