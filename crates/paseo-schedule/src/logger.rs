use std::sync::Mutex;

/// Minimal logging seam (the daemon uses pino; the edge adapts this to its logger).
pub trait ScheduleLogger: Send + Sync {
    fn warn(&self, message: &str);
    fn error(&self, message: &str);
}

pub struct NoopLogger;

impl ScheduleLogger for NoopLogger {
    fn warn(&self, _message: &str) {}
    fn error(&self, _message: &str) {}
}

pub struct StderrLogger;

impl ScheduleLogger for StderrLogger {
    fn warn(&self, message: &str) {
        eprintln!("[schedule] WARN {message}");
    }
    fn error(&self, message: &str) {
        eprintln!("[schedule] ERROR {message}");
    }
}

/// Collects log lines; handy in tests.
#[derive(Default)]
pub struct CollectingLogger {
    pub warnings: Mutex<Vec<String>>,
    pub errors: Mutex<Vec<String>>,
}

impl ScheduleLogger for CollectingLogger {
    fn warn(&self, message: &str) {
        self.warnings.lock().unwrap().push(message.to_string());
    }
    fn error(&self, message: &str) {
        self.errors.lock().unwrap().push(message.to_string());
    }
}
