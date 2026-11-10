//! SET / SHOW search_path are handled in the session (not the stateless utility
//! interceptor) because the path is session state that also feeds unqualified
//! name resolution in the engine.

use crate::session::Session;
use overlite_core::text::{eq_fold, split_words, trim_chars, trim_end_chars};
use overlite_core::{Column, ResultSet, Value};
use std::collections::HashMap;

/// What a search_path statement did.
#[derive(Debug)]
pub(crate) enum SearchPathOutcome {
    /// SET or RESET.
    Changed,
    /// SHOW.
    Shown(ResultSet),
}

impl Session {
    /// Handles SET/SHOW/RESET search_path. None when it is not one of those.
    pub(crate) fn try_search_path(&mut self, sql: &str) -> Option<SearchPathOutcome> {
        let f = split_words(sql);
        if f.len() < 2 {
            return None;
        }
        let head = f[0].to_uppercase();
        // RESET search_path
        if head == "RESET" && eq_fold(strip_semi(f[1]), "search_path") {
            self.search_path = None;
            return Some(SearchPathOutcome::Changed);
        }
        if head == "SHOW" && eq_fold(strip_semi(f[1]), "search_path") {
            return Some(SearchPathOutcome::Shown(ResultSet {
                is_query: true,
                columns: vec![Column {
                    name: "search_path".to_string(),
                    decl_type: "text".to_string(),
                }],
                rows: vec![vec![Value::Text(self.search_path_string())]],
                rows_affected: 1,
                command: "SHOW".into(),
                ..Default::default()
            }));
        }
        // SET [SESSION|LOCAL] search_path (TO|=) <list>
        if head == "SET" {
            let mut rest = &f[1..];
            if !rest.is_empty() && (eq_fold(rest[0], "session") || eq_fold(rest[0], "local")) {
                rest = &rest[1..];
            }
            if rest.len() >= 2 && eq_fold(rest[0], "search_path") && (rest[1] == "=" || eq_fold(rest[1], "to")) {
                self.search_path = parse_search_path_list(&rest[2..].join(" "));
                return Some(SearchPathOutcome::Changed);
            }
        }
        None
    }

    /// Renders the current path for SHOW (defaults like Postgres).
    fn search_path_string(&self) -> String {
        let path = match &self.search_path {
            Some(p) if p.is_empty() => return "\"\"".to_string(),
            Some(p) => p,
            None => return "\"$user\", public".to_string(),
        };
        let quoted: Vec<&str> = path
            .iter()
            .map(|p| if p == "$user" { "\"$user\"" } else { p.as_str() })
            .collect();
        quoted.join(", ")
    }
}

/// Splits a search_path value into schema names, stripping quotes and a trailing
/// semicolon. "DEFAULT" resets to the default (None), as does an empty list.
fn parse_search_path_list(list: &str) -> Option<Vec<String>> {
    let list = trim_end_chars(list, "; \t").trim();
    if eq_fold(list, "default") {
        return None;
    }
    let mut out = Vec::new();
    for part in list.split(',') {
        let p = trim_chars(part.trim(), "\"'");
        if !p.is_empty() {
            out.push(p.to_string());
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn strip_semi(s: &str) -> &str {
    trim_end_chars(s, ";")
}

/// Extracts a search_path set at connection time: either the `search_path`
/// startup parameter (drivers send the connection-string value) or
/// `-c search_path=...` inside the `options` parameter (PGOPTIONS).
pub(crate) fn startup_search_path(params: &HashMap<String, String>) -> Option<Vec<String>> {
    if let Some(v) = params.get("search_path")
        && !v.trim().is_empty()
    {
        return parse_search_path_list(v);
    }
    let opts = params.get("options").map_or("", std::string::String::as_str);
    if opts.is_empty() {
        return None;
    }
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
            && eq_fold(k, "search_path")
        {
            return parse_search_path_list(v);
        }
        i += 1;
    }
    None
}

/// Whether raw is a SET/SHOW/RESET search_path statement (routed specially in the
/// extended path so it updates session state).
pub(crate) fn is_search_path_stmt(raw: &str) -> bool {
    crate::ast::classify::read(raw, crate::ast::classify::search_path).unwrap_or_default()
}
