//! Types shared by the Redis / MongoDB tabs: an operation to run on the worker, optionally behind
//! a confirmation, tagged so the tab can tell what a result is for.

use talus_core::nosql::NoSqlOp;

pub struct Confirmation {
    pub title: String,
    pub message: String,
    pub button: String,
    /// Exact command shown to the user, when there is a readable one.
    pub detail: Option<String>,
}

pub struct NoSqlCmd {
    pub op: NoSqlOp,
    pub tag: String,
    pub confirm: Option<Confirmation>,
}

impl NoSqlCmd {
    pub fn run(op: NoSqlOp, tag: impl Into<String>) -> Self {
        Self { op, tag: tag.into(), confirm: None }
    }

    pub fn confirmed(
        op: NoSqlOp,
        tag: impl Into<String>,
        title: impl Into<String>,
        message: impl Into<String>,
        button: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            op,
            tag: tag.into(),
            confirm: Some(Confirmation { title: title.into(), message: message.into(), button: button.into(), detail }),
        }
    }
}

/// Human-readable TTL: `no expiry`, `42 s`, `3 h 5 min`.
pub fn ttl_label(ttl_ms: Option<i64>) -> String {
    let Some(ms) = ttl_ms else { return "no expiry".into() };
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, (s % 3600) / 60),
        _ => format!("{} d {} h", s / 86_400, (s % 86_400) / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_labels() {
        assert_eq!(ttl_label(None), "no expiry");
        assert_eq!(ttl_label(Some(42_000)), "42 s");
        assert_eq!(ttl_label(Some(125_000)), "2 min 5 s");
        assert_eq!(ttl_label(Some(3_900_000)), "1 h 5 min");
        assert_eq!(ttl_label(Some(200_000_000)), "2 d 7 h");
    }
}
