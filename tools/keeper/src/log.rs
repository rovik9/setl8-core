//! JSON log lines, optional file, optional webhook. Nothing secret ever reaches here: key material is
//! never passed in, and the webhook URL lives only inside [`HttpNotifier`], whose `Debug` redacts it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::alerts::Alert;

/// A webhook delivery failed (no detail on purpose: a URL may hide in it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostFailed;

pub trait Notifier {
    /// POST `body`. The error carries no detail on purpose (a URL may hide in it).
    fn post(&self, body: &str) -> Result<(), PostFailed>;
}

pub struct HttpNotifier {
    url: String,
}

impl HttpNotifier {
    pub fn new(url: &str) -> HttpNotifier {
        HttpNotifier { url: url.to_string() }
    }
}

impl std::fmt::Debug for HttpNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpNotifier(<redacted>)")
    }
}

impl Notifier for HttpNotifier {
    fn post(&self, body: &str) -> Result<(), PostFailed> {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
        agent
            .post(&self.url)
            .set("content-type", "application/json")
            .send_string(body)
            .map(|_| ())
            .map_err(|_| PostFailed)
    }
}

pub struct Logger {
    pub stdout: bool,
    pub file: Option<PathBuf>,
    /// Tests read what was logged here.
    pub capture: Option<Rc<RefCell<Vec<String>>>>,
    pub notifier: Option<Box<dyn Notifier>>,
    /// A repeated alert is POSTed again only after this long (it is always logged).
    pub repeat_secs: i64,
    notified: HashMap<String, i64>,
}

impl Logger {
    pub fn new() -> Logger {
        Logger { stdout: true, file: None, capture: None, notifier: None, repeat_secs: 3600, notified: HashMap::new() }
    }

    pub fn quiet() -> Logger {
        Logger { stdout: false, ..Logger::new() }
    }

    fn emit(&mut self, line: String) {
        if self.stdout {
            println!("{line}");
        }
        if let Some(c) = &self.capture {
            c.borrow_mut().push(line.clone());
        }
        if let Some(p) = &self.file {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    pub fn event(&mut self, level: &str, event: &str, fields: Value) {
        let mut m = Map::new();
        m.insert(
            "ts".into(),
            json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)),
        );
        m.insert("level".into(), json!(level));
        m.insert("event".into(), json!(event));
        if let Value::Object(o) = fields {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        self.emit(Value::Object(m).to_string());
    }

    /// Logs an alert and, if a webhook is configured, posts it (at most once per `repeat_secs` per alert).
    pub fn alert(&mut self, a: &Alert, cluster: &str) {
        let mut fields = json!({"alert": a.kind, "key": a.key, "cluster": cluster});
        if let (Value::Object(f), Value::Object(d)) = (&mut fields, a.detail.clone()) {
            for (k, v) in d {
                f.entry(k).or_insert(v);
            }
        }
        self.event("alert", a.kind, fields.clone());
        let Some(n) = &self.notifier else { return };
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let id = format!("{}:{}", a.kind, a.key);
        if let Some(t) = self.notified.get(&id) {
            if now - *t < self.repeat_secs {
                return;
            }
        }
        if n.post(&fields.to_string()).is_ok() {
            self.notified.insert(id, now);
        } else {
            self.event("warn", "webhook_failed", json!({"note": "delivery failed; the keeper carries on"}));
        }
    }
}

impl Default for Logger {
    fn default() -> Self {
        Logger::new()
    }
}
