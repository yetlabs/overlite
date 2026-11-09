//! The translation of a statement: the fast path, which a point lookup takes, and the
//! tree's translation, from the cache after the first. Not part of `make check`: `cargo bench -p overlite-postgres`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use overlite_postgres::{needs_no_rewrite, rewrite};

/// A lookup the fast path recognizes.
const LOOKUP: &str = "SELECT cnpj, razao_social FROM cnpj WHERE cnpj >= CAST($1 AS text) LIMIT 1";

/// A statement the printer writes anew.
const REWRITTEN: &str = "SELECT id::text, data->>'name' AS name, tags[1], now() - interval '1 day' \
     FROM public.events WHERE name ILIKE 'a%' AND created_at > '2024-01-02 03:04:05+02' OFFSET 10";

fn bench(c: &mut Criterion) {
    c.bench_function("needs_no_rewrite/lookup", |b| {
        b.iter(|| needs_no_rewrite(black_box(LOOKUP)));
    });
    c.bench_function("rewrite/lookup", |b| b.iter(|| rewrite(black_box(LOOKUP))));
    c.bench_function("rewrite/rewritten", |b| b.iter(|| rewrite(black_box(REWRITTEN))));
}

criterion_group!(benches, bench);
criterion_main!(benches);
