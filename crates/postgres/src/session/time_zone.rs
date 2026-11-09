//! The session's TimeZone: SET TIME ZONE, SET timezone, RESET, and the startup
//! parameter. The zone lives in the engine, per thread (`set_session_zone`), where a
//! timestamptz is read and written; the protocol renders one in it, and each change is
//! reported to the client with ParameterStatus, as Postgres reports it.
//!
//! A SET in a transaction that rolls back is undone, and SET LOCAL lasts until the
//! transaction ends, as in Postgres.

use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::SqlState;
use overlite_core::text::{eq_fold, split_words};
use overlite_engine::{session_zone, set_session_zone};
use pg_query::NodeEnum;
use std::collections::HashMap;

/// A change of the TimeZone a statement asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ZoneChange {
    /// SET LOCAL: until the transaction ends.
    local: bool,
    /// The zone, None for the default (UTC).
    zone: Option<String>,
}

/// SET [SESSION | LOCAL] TIME ZONE value, SET [SESSION | LOCAL] timezone {TO | =}
/// value, and RESET timezone, read from the node. None for any other statement.
pub(crate) fn zone_change(sql: &str) -> Option<ZoneChange> {
    if !crate::ast::classify::read(sql, crate::ast::classify::time_zone).unwrap_or(false) {
        return None;
    }
    crate::ddl::tree::read(sql, zone_change_of)
}

fn zone_change_of(n: &NodeEnum) -> Option<ZoneChange> {
    use pg_query::protobuf::VariableSetKind as K;
    let NodeEnum::VariableSetStmt(v) = n else {
        return None;
    };
    let zone = match K::try_from(v.kind).ok()? {
        K::VarSetValue => match v.args.first()?.node.as_ref()? {
            // An INTERVAL: not a zone this server reads, named as written.
            NodeEnum::TypeCast(c) => Some(["INTERVAL ", &crate::ddl::tree::arg_text(c.arg.as_deref()?)?].concat()),
            _ => Some(crate::ddl::tree::arg_text(v.args.first()?)?),
        },
        K::VarSetDefault | K::VarReset => None,
        _ => return None,
    };
    Some(ZoneChange {
        local: v.is_local,
        zone,
    })
}

/// The change of zone sql asks for, read from its text: the classifier the tree's
/// is held to in ast/diff.rs, which goes with the other text classifiers.
#[cfg(test)]
pub(crate) fn zone_change_text(sql: &str) -> Option<ZoneChange> {
    let text = overlite_core::text::trim_end_chars(sql.trim(), ";");
    let f = split_words(text);
    let mut i = 1;
    let head = f.first()?;
    if eq_fold(head, "reset") {
        return (f.len() == 2 && eq_fold(f[1], "timezone")).then_some(ZoneChange {
            local: false,
            zone: None,
        });
    }
    if !eq_fold(head, "set") {
        return None;
    }
    let mut local = false;
    if f.get(i).is_some_and(|w| eq_fold(w, "session")) {
        i += 1;
    } else if f.get(i).is_some_and(|w| eq_fold(w, "local")) {
        local = true;
        i += 1;
    }
    let word = |k: usize, w: &str| f.get(k).is_some_and(|x| eq_fold(x, w));
    if word(i, "time") && word(i + 1, "zone") {
        i += 2;
    } else if word(i, "timezone") {
        i += 1;
        if word(i, "to") || f.get(i).is_some_and(|w| *w == "=") {
            i += 1;
        }
    } else if f.get(i).is_some_and(|w| w.len() > 9 && eq_fold(&w[..9], "timezone=")) {
        // SET timezone='x', written without spaces.
        let value = &f[i][9..];
        return Some(ZoneChange {
            local,
            zone: zone_value(&[value]),
        });
    } else {
        return None;
    }
    let rest: Vec<&str> = f[i..].to_vec();
    if rest.is_empty() {
        return None;
    }
    Some(ZoneChange {
        local,
        zone: zone_value(&rest),
    })
}

/// The zone a SET names: a quoted or bare name, or DEFAULT and LOCAL for the default.
#[cfg(test)]
fn zone_value(words: &[&str]) -> Option<String> {
    let joined = words.join(" ");
    let v = joined.trim();
    if eq_fold(v, "default") || eq_fold(v, "local") {
        return None;
    }
    if let Some(inner) = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return Some(inner.replace("''", "'"));
    }
    Some(crate::session::utility::unquote_ident(v))
}

/// The zone the startup parameters ask for: TimeZone itself, or `-c TimeZone=x` in
/// options.
pub(crate) fn startup_zone(params: &HashMap<String, String>) -> Option<String> {
    if let Some((_, v)) = params
        .iter()
        .find(|(k, v)| eq_fold(k, "timezone") && !v.trim().is_empty())
    {
        return Some(v.trim().to_string());
    }
    let opts = params.get("options")?;
    let flds = split_words(opts);
    let mut i = 0;
    while i < flds.len() {
        let mut f = flds[i];
        if f == "-c" && i + 1 < flds.len() {
            f = flds[i + 1];
            i += 1;
        } else {
            f = f.strip_prefix("-c").unwrap_or(f);
        }
        if let Some((k, v)) = f.split_once('=')
            && eq_fold(k, "timezone")
        {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

impl Session {
    /// Applies a change of TimeZone and reports the new value to the client.
    pub(crate) fn apply_zone_change(&mut self, change: &ZoneChange) -> Result<()> {
        let before = session_zone();
        let zone = change.zone.as_deref().unwrap_or("UTC");
        if zone.trim_start().starts_with(['+', '-']) || zone.trim().parse::<f64>().is_ok() {
            // A number of hours or an INTERVAL: Postgres' own POSIX names for them.
            if zone.trim().parse::<f64>().is_ok() {
                return Err(errf!(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "a time zone given as a number of hours is not supported"
                ));
            }
        }
        let name = set_session_zone(zone)?;
        if self.in_tx() {
            if change.local {
                self.zone_local_restore.get_or_insert(before);
            } else {
                self.zone_tx_restore.get_or_insert(before);
            }
        }
        self.c.send_parameter_status("TimeZone", &name)
    }

    /// A statement changed the TimeZone through set_config: kept for the end of the
    /// transaction as a SET is, and reported.
    pub(crate) fn zone_changed_by_sql(&mut self, before: String, local: bool) -> Result<()> {
        if self.in_tx() {
            if local {
                self.zone_local_restore.get_or_insert(before);
            } else {
                self.zone_tx_restore.get_or_insert(before);
            }
        }
        self.c.send_parameter_status("TimeZone", &session_zone())
    }

    /// At the end of a transaction: SET LOCAL lapses, and a rollback undoes a SET.
    pub(crate) fn end_tx_zone(&mut self, committed: bool) -> Result<()> {
        let local = self.zone_local_restore.take();
        let tx = self.zone_tx_restore.take();
        let back = if committed { local } else { tx.or(local) };
        // A SET after the SET LOCAL in the same transaction stays, as in Postgres.
        let Some(zone) = back else {
            return Ok(());
        };
        if zone == session_zone() {
            return Ok(());
        }
        let name = set_session_zone(&zone)?;
        self.c.send_parameter_status("TimeZone", &name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_forms_of_set() {
        let set = |local, zone: Option<&str>| {
            Some(ZoneChange {
                local,
                zone: zone.map(str::to_string),
            })
        };
        assert_eq!(
            zone_change("SET TIME ZONE 'Africa/Nairobi'"),
            set(false, Some("Africa/Nairobi"))
        );
        assert_eq!(zone_change("set timezone to 'UTC';"), set(false, Some("UTC")));
        assert_eq!(
            zone_change("SET TimeZone = \"Europe/Paris\""),
            set(false, Some("Europe/Paris"))
        );
        assert_eq!(zone_change("SET LOCAL TIME ZONE DEFAULT"), set(true, None));
        assert_eq!(zone_change("SET SESSION TIME ZONE LOCAL"), set(false, None));
        assert_eq!(zone_change("RESET timezone"), set(false, None));
        assert_eq!(zone_change("SET search_path TO x"), None);
    }
}
