mod aggregation;
mod aggregation_charts;
mod aggregation_editor;
mod allocator;
mod app;
mod arrow_types;
mod audio;
mod columnar;
mod controls;
mod datafusion_session;
mod date_picker;
mod display_cache;
mod engine;
mod export;
mod filter_editor;
mod filtering;
mod formatting;
mod help;
mod i18n;
mod icons;
#[cfg(target_os = "macos")]
mod macos_icon;
mod nested;
mod parquet_compat;
mod peek;
mod process_memory;
mod profile_quantiles;
mod profile_stats;
mod resource_budget;
#[cfg(test)]
mod selection;
mod source_guard;
mod source_loader;
mod table_state;
#[cfg(test)]
mod test_support;
mod text_input;
mod theme;
mod window_chrome;

fn main() {
    allocator::configure();
    static LOGGER: StderrLogger = StderrLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    app::run();
}

struct StderrLogger;
impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{} {}: {}", record.level(), record.target(), record.args());
        }
    }
    fn flush(&self) {}
}
