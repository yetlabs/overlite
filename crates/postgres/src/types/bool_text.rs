//! A boolean sent as text is stored as SQLite's 1 or 0. A client that binds
//! without describing first — node-postgres, which also never says a type —
//! sends a JS true as the text "true"; passed through as a string it was stored
//! as that text in a boolean column, and `WHERE flag` or `flag = true` never
//! matched it again. Describe already types such a parameter by its column
//! (param_types); this is the same inference, at Bind, for the parameters that
//! arrive as text.

use crate::session::{Prepared, Session, format_for};
use crate::types::Format;
use crate::types::OID_BOOL;
use overlite_core::Value;

impl Session {
    /// Turns each text-format parameter whose type is boolean — declared by the
    /// client, or inferred from the column it goes into — into 1 or 0. A value
    /// that is not a Postgres boolean is left as it came.
    pub(crate) fn coerce_text_bools(&mut self, prep: &Prepared, fmts: &[Format], params: &mut [Value]) {
        if prep.raw.is_empty() || !any_text_bool(fmts, params) {
            return;
        }
        if prep.bind_oids.borrow().is_none() {
            let oids = self.param_types(&prep.raw, prep.num_params, &prep.param_oids);
            *prep.bind_oids.borrow_mut() = Some(oids);
        }
        let guard = prep.bind_oids.borrow();
        let Some(bind_oids) = guard.as_ref() else {
            return;
        };
        for i in 0..params.len() {
            let b = {
                let Value::Text(str) = &params[i] else { continue };
                if format_for(fmts, i) != Format::Text || i >= bind_oids.len() || bind_oids[i] != OID_BOOL {
                    continue;
                }
                overlite_core::parse_bool(str)
            };
            if let Some(b) = b {
                params[i] = Value::Int(bool_value(b));
            }
        }
    }
}

/// Reports whether some text parameter reads as a boolean, so the column lookup
/// runs only for statements that may need it.
fn any_text_bool(fmts: &[Format], params: &[Value]) -> bool {
    for (i, p) in params.iter().enumerate() {
        if let Value::Text(str) = p
            && format_for(fmts, i) == Format::Text
            && overlite_core::parse_bool(str).is_some()
        {
            return true;
        }
    }
    false
}

fn bool_value(b: bool) -> i64 {
    i64::from(b)
}
