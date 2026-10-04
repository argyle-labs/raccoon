//! orca config rows (`orca config get <noun> <name>`), read through the
//! toolkit's `db.op` capability instead of re-entering the daemon via its CLI.
//!
//! `config_rows` is a core table, reached with an empty namespace the way
//! `plugin_toolkit::core_tables` reaches its tables; core currently logs (but
//! permits) that cross-namespace read.

use plugin_toolkit::abi::{DbOp, DbRow, DbValue};
use plugin_toolkit::runtime::db_op;

/// The `json` of the `noun`/`name` row — the local row first, then the most
/// recently updated replica, matching core's own lookup. `Ok(None)` when unset.
pub fn row_json(noun: &str, name: &str) -> Result<Option<String>, String> {
    let reply = db_op(&DbOp::List {
        namespace: String::new(),
        table: "config_rows".to_string(),
    })
    .map_err(|e| format!("read config {noun}:{name}: {e:#}"))?;
    Ok(pick(&reply.rows, noun, name))
}

fn pick(rows: &[DbRow], noun: &str, name: &str) -> Option<String> {
    let text = |r: &DbRow, c: &str| match r.get(c) {
        Some(DbValue::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let replica = |r: &DbRow| match r.get("is_replica") {
        Some(DbValue::Int(n)) => *n != 0,
        Some(DbValue::Bool(b)) => *b,
        _ => false,
    };
    rows.iter()
        .filter(|r| {
            text(r, "noun").as_deref() == Some(noun) && text(r, "name").as_deref() == Some(name)
        })
        .min_by(|a, b| {
            replica(a)
                .cmp(&replica(b))
                .then_with(|| text(b, "updated_at").cmp(&text(a, "updated_at")))
        })
        .and_then(|r| text(r, "json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(noun: &str, name: &str, json: &str, replica: bool, at: &str) -> DbRow {
        [
            ("noun", DbValue::Text(noun.into())),
            ("name", DbValue::Text(name.into())),
            ("json", DbValue::Text(json.into())),
            ("is_replica", DbValue::Int(replica as i64)),
            ("updated_at", DbValue::Text(at.into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    #[test]
    fn prefers_local_then_newest_replica() {
        let rows = vec![
            row(
                "power",
                "cpu",
                r#"{"mode":"replica-new"}"#,
                true,
                "2026-10-03",
            ),
            row("power", "cpu", r#"{"mode":"local"}"#, false, "2026-10-01"),
            row("power", "gpu", r#"{}"#, false, "2026-10-04"),
        ];
        assert_eq!(
            pick(&rows, "power", "cpu").as_deref(),
            Some(r#"{"mode":"local"}"#)
        );
        let replicas = vec![
            row("g", "n", "old", true, "2026-10-01"),
            row("g", "n", "new", true, "2026-10-02"),
        ];
        assert_eq!(pick(&replicas, "g", "n").as_deref(), Some("new"));
        assert_eq!(pick(&rows, "nope", "cpu"), None);
    }

    #[test]
    fn without_a_capability_sink_the_read_fails_loudly() {
        assert!(row_json("power", "cpu").is_err());
    }
}
