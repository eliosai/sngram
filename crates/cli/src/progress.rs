//! Multi-bar progress display for dataset streaming.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

const BAR_TEMPLATE: &str =
    "  {prefix:<18} {bar:30.cyan/dim} {percent:>3}%  {per_sec:>10}  ETA {eta}";
const TOTAL_TEMPLATE: &str =
    "  {prefix:<18.bold} {bar:30.green/dim} {percent:>3}%  {per_sec:>10}  ETA {eta}";

/// Progress bars with explicit dataset names.
pub struct Progress {
    multi: MultiProgress,
    bars: Vec<ProgressBar>,
    total: ProgressBar,
}

impl Progress {
    #[must_use]
    pub fn named(names: &[&str], file_counts: &[u64]) -> Self {
        let multi = MultiProgress::new();
        let style = ProgressStyle::with_template(BAR_TEMPLATE)
            .unwrap_or_else(|_| ProgressStyle::default_bar());

        let bars: Vec<ProgressBar> = names.iter().zip(file_counts)
            .map(|(&name, &count)| {
                let pb = multi.add(ProgressBar::new(count));
                pb.set_style(style.clone());
                pb.set_prefix(name.to_owned());
                pb
            })
            .collect();

        let total_count: u64 = file_counts.iter().sum();
        let total = multi.add(ProgressBar::new(total_count));
        let ts = ProgressStyle::with_template(TOTAL_TEMPLATE)
            .unwrap_or_else(|_| ProgressStyle::default_bar());
        total.set_style(ts);
        total.set_prefix("total");

        Self { multi, bars, total }
    }

    pub fn inc_bytes(&self, idx: usize, n: u64) {
        if let Some(bar) = self.bars.get(idx) { bar.inc(n); }
        self.total.inc(n);
    }

    pub fn finish_dataset(&self, idx: usize) {
        if let Some(bar) = self.bars.get(idx) { bar.finish(); }
    }

    pub fn finish_all(&self) {
        for bar in &self.bars { bar.finish(); }
        self.total.finish();
    }
}
