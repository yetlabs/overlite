//! Matches an explicit `public.` qualifier, quoted or not. Clients think their
//! tables live in schema public; in SQLite they live in the unqualified main
//! schema, so the qualifier is dropped.

use crate::resolve::schema_qualify::{builder_string, is_ident_byte, skip_literal};
use overlite_core::lazy_regex;

lazy_regex!(RE_PUBLIC_QUALIFIER, r#"(?i)"?\bpublic\b"?\."#);

/// Removes an explicit public schema qualifier, outside string and identifier
/// literals. Runs after search_path resolution, not before: `public.t` names the
/// public table even when the path points elsewhere.
pub(crate) fn strip_public_qualifier(query: &str) -> String {
    if !query.contains('.') {
        return query.to_string();
    }
    let q = query.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < q.len() {
        let c = q[i];
        if c == b'\'' {
            let j = skip_literal(query, i, b'\'');
            b.extend_from_slice(&q[i..j]);
            i = j;
            continue;
        }
        // A quoted identifier is only skipped when it is not the public
        // qualifier itself: `"public"."t"` must still lose its first half.
        if c == b'"' {
            if let Some(loc) = RE_PUBLIC_QUALIFIER.find(&query[i..])
                && loc.start() == 0
            {
                i += loc.end();
                continue;
            }
            let j = skip_literal(query, i, b'"');
            b.extend_from_slice(&q[i..j]);
            i = j;
            continue;
        }
        if (i == 0 || (!is_ident_byte(q[i - 1]) && q[i - 1] != b'.'))
            && (c == b'p' || c == b'P')
            && let Some(loc) = RE_PUBLIC_QUALIFIER.find(&query[i..])
            && loc.start() == 0
        {
            i += loc.end();
            continue;
        }
        b.push(c);
        i += 1;
    }
    builder_string(b)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn strip_public_qualifier_cases() {
        for (input, want) in [
            ("SELECT * FROM public.clientes", "SELECT * FROM clientes"),
            (r#"SELECT * FROM "public"."clientes""#, r#"SELECT * FROM "clientes""#),
            (r#"SELECT * FROM public."clientes""#, r#"SELECT * FROM "clientes""#),
            (r#"SELECT * FROM "public".clientes"#, "SELECT * FROM clientes"),
            ("SELECT * FROM PUBLIC.clientes", "SELECT * FROM clientes"),
            (
                "SELECT * FROM t WHERE x = 'public'",
                "SELECT * FROM t WHERE x = 'public'",
            ), // string untouched
            (
                "SELECT * FROM t WHERE x = 'public.y'",
                "SELECT * FROM t WHERE x = 'public.y'",
            ),
            ("SELECT * FROM mypublic.t", "SELECT * FROM mypublic.t"), // not a word match
            ("SELECT * FROM t", "SELECT * FROM t"),                   // nothing to do
            (r#"SELECT "notpublic"."t""#, r#"SELECT "notpublic"."t""#),
        ] {
            assert_eq!(strip_public_qualifier(input), want, "strip_public_qualifier({input:?})");
        }
    }
}
