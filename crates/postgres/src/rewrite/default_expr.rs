//! SQLite only accepts a bare literal or a keyword like CURRENT_TIMESTAMP as a
//! column default; anything computed has to be parenthesised. Postgres needs no
//! parentheses, so `DEFAULT now()` and `DEFAULT gen_random_uuid()` reached SQLite
//! as a syntax error on the opening bracket.

/// Returns the index just past the quoted run starting at i.
pub(crate) fn skip_quoted(s: &str, i: usize, q: u8) -> usize {
    let b = s.as_bytes();
    let mut i = i + 1;
    while i < b.len() {
        if b[i] == q {
            if i + 1 < b.len() && b[i + 1] == q {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}
