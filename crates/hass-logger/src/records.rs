//! Shared record normalization for console output, filtering and capture.

use std::{collections::BTreeMap, fmt};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

use crate::LoggerLevel;

/// An actually emitted tracing record with native and LogTracer fields resolved.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub target: String,
    pub message: String,
    pub level: LoggerLevel,
    /// TRACE retains its native numeric value below Python DEBUG.
    pub severity_number: i64,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub exception: String,
    pub fields: BTreeMap<String, String>,
    pub spans: Vec<String>,
}

impl EventRecord {
    /// The common console/raw-stream line for an emitted record.
    pub fn format_line(&self, timestamp: std::time::SystemTime) -> String {
        use std::fmt::Write;
        let mut line = format!(
            "{} {} [{}] ",
            chrono::DateTime::<chrono::Utc>::from(timestamp)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            if self.severity_number == 5 {
                "TRACE"
            } else {
                self.level.as_str()
            },
            self.target,
        );
        for span in &self.spans {
            let _ = write!(line, "{span}: ");
        }
        line.push_str(&self.message);
        for (key, value) in &self.fields {
            let _ = write!(line, " {key}={value}");
        }
        if !self.exception.is_empty() {
            line.push('\n');
            line.push_str(&self.exception);
        }
        line
    }
}

/// Resolve dynamic logger/source metadata before any output or capture filtering.
pub fn event_record(event: &Event<'_>) -> EventRecord {
    let metadata = event.metadata();
    let mut visitor = RecordVisitor::default();
    event.record(&mut visitor);
    let mut fields = visitor.fields;
    let target = fields
        .remove("hass.target")
        .or_else(|| fields.remove("log.target"))
        .unwrap_or_else(|| metadata.target().to_owned());
    let explicit_level = fields.remove("hass.level");
    let native_level = match *metadata.level() {
        Level::ERROR => LoggerLevel::Error,
        Level::WARN => LoggerLevel::Warning,
        Level::INFO => LoggerLevel::Info,
        _ => LoggerLevel::Debug,
    };
    // A marker distinguishes CRITICAL from ERROR without promoting DEBUG/INFO.
    let level = match explicit_level.as_deref() {
        Some("CRITICAL" | "FATAL") if native_level == LoggerLevel::Error => LoggerLevel::Critical,
        _ => native_level,
    };
    let file = fields
        .remove("hass.file")
        .or_else(|| fields.remove("log.file"))
        .or_else(|| metadata.file().map(str::to_owned));
    let line = fields
        .remove("hass.line")
        .or_else(|| fields.remove("log.line"))
        .and_then(|value| value.parse().ok())
        .or_else(|| metadata.line());
    fields.remove("log.module_path");
    let message = fields.remove("message").unwrap_or_default();
    let exception = fields.remove("exception").unwrap_or_default();
    let severity_number = if *metadata.level() == Level::TRACE {
        5
    } else {
        level.number()
    };
    EventRecord {
        target,
        message,
        level,
        severity_number,
        file,
        line,
        exception,
        fields,
        spans: Vec::new(),
    }
}

#[derive(Default)]
struct RecordVisitor {
    fields: BTreeMap<String, String>,
}
impl Visit for RecordVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
}

/// Console format retaining requested logger/severity and structured fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct NormalizedEventFormat;
impl<S, N> FormatEvent<S, N> for NormalizedEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut record = event_record(event);
        if let Some(scope) = context.event_scope() {
            for span in scope.from_root() {
                let mut text = span.name().to_owned();
                let extensions = span.extensions();
                if let Some(fields) =
                    extensions.get::<tracing_subscriber::fmt::FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    use std::fmt::Write;
                    write!(text, "{{{fields}}}")?;
                }
                record.spans.push(text);
            }
        }
        writeln!(
            writer,
            "{}",
            record.format_line(std::time::SystemTime::now())
        )
    }
}
