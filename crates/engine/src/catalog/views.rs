//! The catalog views that span every attached schema.

use crate::catalog::schema::{CATALOG_ROLE, SchemaRef};
use crate::catalog::{info_routines_view, lower_chars, pg_proc_view};
use overlite_core::lazy_regex;
use overlite_core::text::quote_literal;

// `CREATE TEMP VIEW <name> AS ` prefix and the body.
lazy_regex!(
    RE_VIEW_HEADER,
    r#"(?is)^(\s*CREATE\s+TEMP\s+VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:"[^"]+"|\w+)\s+AS\s+)(.*)$"#
);

/// Instantiates a per-schema SQL fragment: @DB@ = attached db name (pragma
/// scope), @MASTER@ = the schema's table source (an sqlite_master or a filtered
/// view of it), @NS@ = namespace oid, @OFF@ = oid offset (keeps ids distinct
/// across schemas), @PG@ = the Postgres schema name. One pass: what was
/// substituted is not rescanned.
fn frag(tmpl: &str, r: &SchemaRef, db_name: &str, enum_base: i64) -> String {
    let tmpl: &str = &tmpl.replace("@SERIAL_DEFAULT@", SERIAL_DEFAULT);
    let pairs: [(&str, String); 10] = [
        // The schema of an undotted name in the schema's file: a single-file schema's
        // tables are dotted names beside public's.
        (
            "@FILESCHEMA@",
            if r.prefix.is_empty() {
                r.pg_name.clone()
            } else {
                "public".to_string()
            },
        ),
        ("@DB@", r.db.clone()),
        ("@MASTER@", r.master()),
        ("@NS@", r.ns_oid.to_string()),
        ("@OFF@", r.offset.to_string()),
        ("@PG@", r.pg_name.clone()),
        // @PLEN@ strips a single-file schema's "vendas." table-name prefix from
        // displayed relation names; it's 1 (a no-op, substr(name,1)=name) for
        // public and for multi-file schemas.
        ("@PLEN@", (r.prefix.len() + 1).to_string()),
        ("@ROLE@", quote_literal(&CATALOG_ROLE)),
        ("@ENUMBASE@", enum_base.to_string()),
        // The catalog columns must name the database the client sees from
        // current_database(); reporting SQLite's internal "main" made every
        // information_schema query filtered by catalog return nothing, which
        // is how gorm's migrator asks whether a table's columns exist.
        ("@DBNAME@", quote_literal(db_name)),
    ];
    let mut out = String::with_capacity(tmpl.len() + tmpl.len() / 2);
    let mut rest = tmpl;
    'scan: while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        for (old, new) in &pairs {
            if rest.starts_with(old) {
                out.push_str(new);
                rest = &rest[old.len()..];
                continue 'scan;
            }
        }
        out.push('@');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn union(name: &str, refs: &[SchemaRef], tmpl: &str, db_name: &str, enum_base: i64) -> String {
    let parts: Vec<String> = refs.iter().map(|r| frag(tmpl, r, db_name, enum_base)).collect();
    format!("CREATE TEMP VIEW {} AS\n{}", name, parts.join("\nUNION ALL\n"))
}

/// Wraps a "CREATE TEMP VIEW ... AS <body>" so the view exposes a tableoid column
/// (pg_dump selects it from every catalog). Views that already define one (e.g.
/// pg_type) are left untouched.
pub(crate) fn with_table_oid(stmt: &str) -> String {
    let Some(m) = RE_VIEW_HEADER.captures(stmt) else {
        return stmt.to_string();
    };
    if lower_chars(&m[2]).contains("tableoid") {
        return stmt.to_string();
    }
    format!("{}SELECT _s.*, 0 AS tableoid FROM ({}) _s", &m[1], &m[2])
}

/// union() such that the view exposes extra_cols (constant columns pg_dump
/// selects that aren't worth threading through every UNION branch).
fn union_wrap(name: &str, extra_cols: &str, refs: &[SchemaRef], tmpl: &str, db_name: &str, enum_base: i64) -> String {
    let parts: Vec<String> = refs.iter().map(|r| frag(tmpl, r, db_name, enum_base)).collect();
    format!(
        "CREATE TEMP VIEW {} AS SELECT _u.*, {} FROM (\n{}\n) _u",
        name,
        extra_cols,
        parts.join("\nUNION ALL\n")
    )
}

/// union_wrap with just a constant tableoid column (the catalog's own oid in
/// pg_class), a row's originating-catalog discriminator for pg_dump.
fn union_oid(name: &str, table_oid: i32, refs: &[SchemaRef], tmpl: &str, db_name: &str, enum_base: i64) -> String {
    union_wrap(
        name,
        &format!("{table_oid} AS tableoid"),
        refs,
        tmpl,
        db_name,
        enum_base,
    )
}

/// The catalog views that span every attached schema. Rebuilt whenever the set
/// of schemas changes.
pub(crate) fn dynamic_catalog_views(refs: &[SchemaRef], db_name: &str, enum_base: i64) -> Vec<String> {
    // pg_namespace: fixed system schemas plus one row per real schema.
    let mut ns = String::from(
        "CREATE TEMP VIEW pg_namespace AS SELECT 11 AS oid,'pg_catalog' AS nspname,10 AS nspowner, 2615 AS tableoid, NULL AS nspacl UNION ALL SELECT 2201,'information_schema',10,2615,NULL",
    );
    for r in refs {
        ns.push_str(&format!(" UNION ALL SELECT {},'{}',10,2615,NULL", r.ns_oid, r.pg_name));
    }

    vec![
        ns,
        pg_class_view(refs, db_name, enum_base),
        union_oid("pg_attribute", 1249, refs, PG_ATTRIBUTE_TMPL, db_name, enum_base),
        union_oid("pg_attrdef", 2604, refs, PG_ATTRDEF_TMPL, db_name, enum_base),
        union_wrap(
            "pg_index",
            "2610 AS tableoid, 0 AS indnullsnotdistinct",
            refs,
            PG_INDEX_TMPL,
            db_name,
            enum_base,
        ),
        union_wrap(
            "pg_constraint",
            "2606 AS tableoid, NULL AS conparentid2, 0 AS conperiod",
            refs,
            PG_CONSTRAINT_TMPL,
            db_name,
            enum_base,
        ),
        union_wrap(
            "pg_trigger",
            "2620 AS tableoid, 0 AS tgparentid",
            refs,
            PG_TRIGGER_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.tables""#,
            refs,
            INFO_TABLES_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.columns""#,
            refs,
            INFO_COLUMNS_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.column_privileges""#,
            refs,
            INFO_COLUMN_PRIVILEGES_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.table_constraints""#,
            refs,
            INFO_TABLE_CONSTRAINTS_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.key_column_usage""#,
            refs,
            INFO_KEY_COLUMN_USAGE_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.referential_constraints""#,
            refs,
            INFO_REFERENTIAL_CONSTRAINTS_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.constraint_column_usage""#,
            refs,
            INFO_CONSTRAINT_COLUMN_USAGE_TMPL,
            db_name,
            enum_base,
        ),
        union(
            r#""information_schema.views""#,
            refs,
            INFO_VIEWS_TMPL,
            db_name,
            enum_base,
        ),
        union("pg_tables", refs, PG_TABLES_TMPL, db_name, enum_base),
        union("pg_indexes", refs, PG_INDEXES_TMPL, db_name, enum_base),
        union("pg_views", refs, PG_VIEWS_TMPL, db_name, enum_base),
        union("pg_matviews", refs, PG_MATVIEWS_TMPL, db_name, enum_base),
        info_schemata_view(refs, db_name),
        union(
            "_overlite_serial_sequences",
            refs,
            SERIAL_SEQUENCES_TMPL,
            db_name,
            enum_base,
        ),
        info_sequences_view(db_name),
        info_check_constraints_view(db_name),
        info_routines_view(db_name),
        pg_proc_view(),
    ]
}

/// Lists every schema (system + user) as information_schema.schemata.
fn info_schemata_view(refs: &[SchemaRef], db_name: &str) -> String {
    let (db, role) = (quote_literal(db_name), quote_literal(&CATALOG_ROLE));
    let mut v = format!(
        r#"CREATE TEMP VIEW "information_schema.schemata" AS SELECT {db} AS catalog_name, 'information_schema' AS schema_name, {role} AS schema_owner UNION ALL SELECT {db},'pg_catalog',{role}"#
    );
    for r in refs {
        v.push_str(&format!(" UNION ALL SELECT {},'{}',{}", db, r.pg_name, role));
    }
    v
}

/// Exposes the internal sequence table as information_schema.sequences.
fn info_sequences_view(db_name: &str) -> String {
    format!(
        "{}{}{}{}{}",
        r#"CREATE TEMP VIEW "information_schema.sequences" AS
	 SELECT "#,
        quote_literal(db_name),
        r#" AS sequence_catalog, 'public' AS sequence_schema,
	        seqname AS sequence_name, 'bigint' AS data_type, 64 AS numeric_precision,
	        2 AS numeric_precision_radix, 0 AS numeric_scale,
	        CAST(start_value AS TEXT) AS start_value, CAST(min_value AS TEXT) AS minimum_value,
	        CAST(max_value AS TEXT) AS maximum_value, CAST(increment AS TEXT) AS increment,
	        CASE is_cycled WHEN 1 THEN 'YES' ELSE 'NO' END AS cycle_option
	 FROM _overlite_sequences
	 UNION ALL SELECT "#,
        quote_literal(db_name),
        r#", schemaname, seqname, 'bigint', 64, 2, 0, '1', '1', '9223372036854775807', '1', 'NO'
	 FROM _overlite_serial_sequences"#
    )
}

/// The sequence Postgres makes for each serial column, `<table>_<column>_seq`. A
/// serial primary key is SQLite's AUTOINCREMENT rowid, which has no sequence of its
/// own to list: this names one for each, unless a real sequence has the name.
/// pg_class, pg_sequence, pg_sequences and information_schema.sequences read it.
const SERIAL_SEQUENCES_TMPL: &str = r#"SELECT '@PG@' AS schemaname,
 substr(m.name,@PLEN@) || '_' || ti.name || '_seq' AS seqname,
 CAST(m.rowid + 86000000 + @OFF@ AS INTEGER) AS oid, @NS@ AS ns, m.name AS tablename
FROM @MASTER@ m JOIN pragma_table_xinfo(m.name,'@DB@') ti
WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'
  AND ti.pk = 1 AND upper(ti.type) = 'INTEGER' AND upper(m.sql) LIKE '%AUTOINCREMENT%'
  AND NOT EXISTS (SELECT 1 FROM _overlite_sequences s
                  WHERE s.seqname = substr(m.name,@PLEN@) || '_' || ti.name || '_seq')"#;

/// Present but empty (we don't enumerate CHECK clauses yet). info_routines_view
/// is defined in catalog/mod.rs from the function list.
fn info_check_constraints_view(db_name: &str) -> String {
    format!(
        "{}{}{}",
        r#"CREATE TEMP VIEW "information_schema.check_constraints" AS
	 SELECT "#,
        quote_literal(db_name),
        r#" AS constraint_catalog, 'public' AS constraint_schema,
	        '' AS constraint_name, '' AS check_clause WHERE 0"#
    )
}

/// pg_class over every schema, plus the sequences (relkind 'S') that live in the
/// main/public database, so \ds and \d <seq> find them.
fn pg_class_view(refs: &[SchemaRef], db_name: &str, enum_base: i64) -> String {
    let mut parts = Vec::with_capacity(refs.len() + 1);
    for r in refs {
        parts.push(frag(PG_CLASS_TMPL, r, db_name, enum_base));
    }
    for r in refs {
        parts.push(frag(PG_UNIQUE_INDEX_CLASS_TMPL, r, db_name, enum_base));
    }
    for r in refs {
        if r.pg_name == "public" {
            parts.push(frag(PG_SEQUENCE_CLASS_TMPL, r, db_name, enum_base));
        }
    }
    parts.push(PG_SERIAL_SEQUENCE_CLASS.to_string());
    format!(
        "CREATE TEMP VIEW pg_class AS SELECT _c.*, 1259 AS tableoid, 0 AS relfrozenxid, 0 AS relhasoids, NULL AS relpartbound2, 0 AS relallfrozen FROM (\n{}\n) _c",
        parts.join("\nUNION ALL\n")
    )
}

// fk_name_expr names the foreign key fk (one pragma_foreign_key_list row of tbl)
// as constraints.rs recovers it; pk_name_expr names tbl's primary key. The
// action expressions map SQLite's spelled-out referential actions to
// pg_constraint's one-letter codes. The opclass expressions give the default
// operator class of an index column (ii, a pragma_index_info row of tbl) and of
// a primary-key column (cid of tbl). They are macros so the templates below can
// concat! them at compile time.

macro_rules! fk_name_expr {
    () => {
        r#"overlite_fk_name(tbl.sql, substr(tbl.name,@PLEN@),
 (SELECT group_concat("from", ',') FROM (SELECT "from" FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq)),
 fk."table", fk.id)"#
    };
}

// How fk is checked, as overlite_fk_deferral reads it from the DDL: 0, 1 deferrable,
// 2 deferrable and initially deferred.
macro_rules! fk_deferral_expr {
    () => {
        r#"overlite_fk_deferral(tbl.sql,
 (SELECT group_concat("from", ',') FROM (SELECT "from" FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq)),
 fk."table", fk.id)"#
    };
}

// The name of the UNIQUE constraint behind il, one pragma_index_list row of tbl.
macro_rules! unique_name_expr {
    () => {
        r#"overlite_unique_name(tbl.sql, substr(tbl.name,@PLEN@),
 (SELECT group_concat(ii.name, ',') FROM pragma_index_info(il.name,'@DB@') ii))"#
    };
}

macro_rules! pk_name_expr {
    () => {
        r#"overlite_pk_name(tbl.sql, substr(tbl.name,@PLEN@))"#
    };
}

macro_rules! fk_update_action_expr {
    () => {
        r#"CASE fk.on_update WHEN 'CASCADE' THEN 'c' WHEN 'SET NULL' THEN 'n'
 WHEN 'SET DEFAULT' THEN 'd' WHEN 'RESTRICT' THEN 'r' ELSE 'a' END"#
    };
}

macro_rules! fk_delete_action_expr {
    () => {
        r#"CASE fk.on_delete WHEN 'CASCADE' THEN 'c' WHEN 'SET NULL' THEN 'n'
 WHEN 'SET DEFAULT' THEN 'd' WHEN 'RESTRICT' THEN 'r' ELSE 'a' END"#
    };
}

macro_rules! index_col_opclass_expr {
    () => {
        r#"overlite_opclass((SELECT t.type FROM pragma_table_xinfo(tbl.name,'@DB@') t WHERE t.cid = ii.cid))"#
    };
}

macro_rules! pk_col_opclass_expr {
    () => {
        r#"overlite_opclass((SELECT t.type FROM pragma_table_xinfo(tbl.name,'@DB@') t WHERE t.cid = pk.cid))"#
    };
}

const PG_TRIGGER_TMPL: &str = r#"SELECT CAST(trg.rowid + 70000000 + @OFF@ AS INTEGER) AS oid,
 CAST(tbl.rowid + @OFF@ AS INTEGER) AS tgrelid, trg.name AS tgname, 0 AS tgfoid, 0 AS tgtype,
 'O' AS tgenabled, 0 AS tgisinternal, 0 AS tgconstrrelid, 0 AS tgconstrindid, 0 AS tgconstraint,
 0 AS tgdeferrable, 0 AS tginitdeferred, 0 AS tgnargs, '' AS tgattr, '' AS tgargs,
 NULL AS tgqual, NULL AS tgoldtable, NULL AS tgnewtable
FROM @MASTER@ trg
JOIN @MASTER@ tbl ON tbl.name = trg.tbl_name AND tbl.type = 'table'
WHERE trg.type = 'trigger' AND trg.name NOT LIKE 'sqlite_%' AND trg.name NOT GLOB '_overlite_*'"#;

const PG_UNIQUE_INDEX_CLASS_TMPL: &str = concat!(
    r#"SELECT CAST(tbl.rowid*1000 + il.seq + 84000000 + @OFF@ AS INTEGER) AS oid,
 "#,
    unique_name_expr!(),
    r#" AS relname,
 @NS@ AS relnamespace,
 0,0,10,403,0,0,0,0,0,0,
 0, 0, 'p', 'i',
 (SELECT count(*) FROM pragma_index_info(il.name,'@DB@')), 0,0,0,0,0,0,1,'n',0,NULL,NULL,NULL,0,0
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1"#
);

const PG_SEQUENCE_CLASS_TMPL: &str = r#"SELECT CAST(seq.rowid + 80000000 + @OFF@ AS INTEGER) AS oid, seq.seqname AS relname, @NS@ AS relnamespace,
 0,0,10,0,0,0,0,0,0,0,
 0, 0, 'p', 'S',
 1, 0,0,0,0,0,0,1,'d',0,NULL,NULL,NULL,0,0
FROM _overlite_sequences seq"#;

const PG_SERIAL_SEQUENCE_CLASS: &str = r#"SELECT oid, seqname, ns,
 0,0,10,0,0,0,0,0,0,0,
 0, 0, 'p', 'S',
 1, 0,0,0,0,0,0,1,'d',0,NULL,NULL,NULL,0,0
FROM _overlite_serial_sequences"#;

const PG_CLASS_TMPL: &str = concat!(
    r#"SELECT CAST(m.rowid + @OFF@ AS INTEGER) AS oid, substr(m.name,@PLEN@) AS relname, @NS@ AS relnamespace,
 CASE WHEN m.type='index' THEN 0 ELSE CAST(m.rowid + 60000000 + @OFF@ AS INTEGER) END AS reltype,
 0 AS reloftype, 10 AS relowner,
 CASE WHEN m.type='index' THEN COALESCE((SELECT am.oid FROM pg_am am
   WHERE instr(m.sql, '/*overlite using ' || am.amname || '*/')), 403) ELSE 0 END AS relam,
 0 AS relfilenode, 0 AS reltablespace,
 0 AS relpages, 0 AS reltuples, 0 AS relallvisible, 0 AS reltoastrelid,
 CASE WHEN m.type='table' AND (EXISTS(SELECT 1 FROM pragma_index_list(m.name,'@DB@'))
       OR EXISTS(SELECT 1 FROM pragma_table_xinfo(m.name,'@DB@') WHERE pk>0)) THEN 1 ELSE 0 END AS relhasindex,
 0 AS relisshared, 'p' AS relpersistence,
 CASE m.type WHEN 'view' THEN 'v' WHEN 'index' THEN 'i'
   ELSE CASE WHEN EXISTS(SELECT 1 FROM @MASTER@ mv WHERE mv.type = 'view' AND mv.name = '_overlite_mv_' || m.name)
   THEN 'm' ELSE 'r' END END AS relkind,
 (SELECT count(*) FROM pragma_table_xinfo(m.name,'@DB@')) AS relnatts,
 -- The CHECK constraints pg_constraint lists: psql's \d and pg_dump read them only
 -- when this counts them.
 CASE WHEN m.type = 'table' THEN (SELECT count(*) FROM json_each(overlite_checks(m.sql, substr(m.name,@PLEN@))) ck
   WHERE NOT (json_extract(ck.value, '$.enum') AND EXISTS (SELECT 1 FROM _overlite_enum_types et
    WHERE (SELECT group_concat(label) FROM (SELECT label FROM _overlite_enums WHERE typname = et.typname ORDER BY sortorder))
          = overlite_enum_labels(m.sql, json_extract(ck.value, '$.cols[0]'))))) ELSE 0 END AS relchecks,
 0 AS relhasrules,
 CASE WHEN EXISTS(SELECT 1 FROM @MASTER@ trg WHERE trg.type='trigger' AND trg.tbl_name=m.name)
       OR (m.type='table' AND EXISTS(SELECT 1 FROM pragma_foreign_key_list(m.name,'@DB@')))
      THEN 1 ELSE 0 END AS relhastriggers,
 0 AS relhassubclass,
 COALESCE((SELECT enabled FROM _overlite_rls rls WHERE lower(rls.tablename)=lower(m.name)),0) AS relrowsecurity,
 COALESCE((SELECT forced FROM _overlite_rls rls WHERE lower(rls.tablename)=lower(m.name)),0) AS relforcerowsecurity,
 1 AS relispopulated, 'd' AS relreplident, 0 AS relispartition,
 NULL AS relacl, overlite_reloptions(m.sql) AS reloptions, NULL AS relpartbound, 0 AS relrewrite, 0 AS relminmxid
FROM @MASTER@ m
WHERE m.type IN ('table','view','index') AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'
UNION ALL
SELECT CAST(tbl.rowid + 90000000 + @OFF@ AS INTEGER), "#,
    pk_name_expr!(),
    r#", @NS@, 0,0,10,403,0,0,0,0,0,0,0,0,'p','i',
 (SELECT count(*) FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0),
 0,0,0,0,0,0,1,'n',0,NULL,NULL,NULL,0,0
FROM @MASTER@ tbl
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND EXISTS(SELECT 1 FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0)"#
);

const PG_ATTRIBUTE_TMPL: &str = r#"SELECT CAST(m.rowid + @OFF@ AS INTEGER) AS attrelid, ti.name AS attname,
 COALESCE(
  (SELECT t.rowid + @ENUMBASE@ FROM _overlite_enum_types t
    WHERE (SELECT group_concat(label) FROM (SELECT label FROM _overlite_enums
             WHERE typname = t.typname ORDER BY sortorder))
          = overlite_enum_labels((SELECT sql FROM @MASTER@ WHERE name = m.name), ti.name)),
  overlite_type_oid(COALESCE(overlite_identity_type(m.sql, ti.name), ti.type))) AS atttypid,
 -1 AS attstattarget, -1 AS attlen, ti.cid + 1 AS attnum,
 (length(ti.type) - length(replace(ti.type, '[]', ''))) / 2 AS attndims, -1 AS attcacheoff,
 overlite_typmod(ti.type) AS atttypmod,
 0 AS attbyval, 'p' AS attstorage, 'i' AS attalign,
 CASE WHEN ti."notnull"=1 OR ti.pk>0 THEN 1 ELSE 0 END AS attnotnull,
 CASE WHEN ti.dflt_value IS NOT NULL OR @SERIAL_DEFAULT@ IS NOT NULL THEN 1 ELSE 0 END AS atthasdef,
 0 AS atthasmissing, overlite_attidentity(m.sql, ti.name) AS attidentity,
 CASE ti.hidden WHEN 2 THEN 'v' WHEN 3 THEN 's' ELSE '' END AS attgenerated, 0 AS attisdropped, 1 AS attislocal,
 0 AS attinhcount,
 COALESCE((SELECT co.oid FROM pg_collation co WHERE co.collname = overlite_column_collation(m.sql, ti.name)),
   CASE WHEN overlite_type_oid(ti.type) IN (25, 1043, 1042) THEN 100 ELSE 0 END) AS attcollation,
 (SELECT '{' || group_concat(item, ',') || '}' FROM (
   SELECT overlite_aclitem(g.grantee, group_concat(g.privilege), COALESCE(
     (SELECT o.owner FROM _overlite_owners o WHERE o.tablename = g.tablename), @ROLE@)) AS item
   FROM _overlite_column_grants g
   WHERE g.tablename = (CASE WHEN '@PG@' = 'public' THEN m.name ELSE '@PG@.' || substr(m.name, @PLEN@) END)
     AND g.columnname = ti.name
   GROUP BY lower(g.grantee) ORDER BY min(g.rowid))) AS attacl, NULL AS attoptions,
 '' AS attcompression, NULL AS attmissingval, 0 AS attispartkey, 0 AS attstorage2
FROM @MASTER@ m JOIN pragma_table_xinfo(m.name,'@DB@') ti
WHERE m.type IN ('table','view') AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'"#;

/// Each column privilege someone holds: those granted on the column, those granted on
/// its table (which cover every column), and the owner's, which are all of them and
/// grantable, as Postgres reports a table whose ACL is the default. The grantor is
/// the owner. A row shows when the current role acts as its grantor or grantee, or
/// the grantee is PUBLIC, as in Postgres.
const INFO_COLUMN_PRIVILEGES_TMPL: &str = r#"SELECT grantor, grantee, table_catalog, table_schema, table_name,
 column_name, privilege_type, is_grantable FROM (
SELECT DISTINCT COALESCE(o.owner, @ROLE@) AS grantor,
 CASE WHEN lower(p.grantee) = 'public' THEN 'PUBLIC' ELSE p.grantee END AS grantee,
 @DBNAME@ AS table_catalog, '@PG@' AS table_schema, substr(m.name, @PLEN@) AS table_name,
 ti.name AS column_name, p.privilege AS privilege_type, p.grantable AS is_grantable
FROM @MASTER@ m JOIN pragma_table_xinfo(m.name,'@DB@') ti
JOIN (
  SELECT tablename, columnname, grantee, upper(privilege) AS privilege, 'NO' AS grantable
    FROM _overlite_column_grants
  UNION ALL
  SELECT g.tablename, NULL, g.grantee, k.p, 'NO' FROM _overlite_grants g
    JOIN (SELECT 'SELECT' AS p UNION ALL SELECT 'INSERT' UNION ALL SELECT 'UPDATE' UNION ALL SELECT 'REFERENCES') k
      ON upper(g.privilege) IN (k.p, 'ALL')
  UNION ALL
  SELECT ow.tablename, NULL, ow.owner, k.p, 'YES' FROM _overlite_owners ow
    JOIN (SELECT 'SELECT' AS p UNION ALL SELECT 'INSERT' UNION ALL SELECT 'UPDATE' UNION ALL SELECT 'REFERENCES') k
) p ON p.tablename = (CASE WHEN '@PG@' = 'public' THEN m.name ELSE '@PG@.' || substr(m.name, @PLEN@) END)
   AND (p.columnname IS NULL OR p.columnname = ti.name)
LEFT JOIN _overlite_owners o ON o.tablename = p.tablename
WHERE m.type IN ('table','view') AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'
  AND ti.hidden <> 1
) WHERE grantee = 'PUBLIC' OR overlite_role_visible(grantee) OR overlite_role_visible(grantor)"#;

/// A column's default that SQLite's DEFAULT does not hold: a generated column's
/// expression, and a nextval default (a serial key is the rowid, and a nextval default
/// is kept in `_overlite_defaults`), as Postgres writes it,
/// `nextval('t_id_seq'::regclass)`, so pg_dump restores it. Over `m` (the table's
/// sqlite_master row) and `ti` (its column), in a template.
const SERIAL_DEFAULT: &str = r#"COALESCE(
 CASE WHEN ti.hidden IN (2, 3) THEN overlite_generated_expr(m.sql, ti.name) END,
 (SELECT 'nextval(''' || d.seqname || '''::regclass)' FROM _overlite_defaults d
   WHERE d.tablename = m.name AND lower(d.colname) = lower(ti.name) LIMIT 1),
 (SELECT 'nextval(''' || CASE s.schemaname WHEN 'public' THEN '' ELSE s.schemaname || '.' END
    || s.seqname || '''::regclass)' FROM _overlite_serial_sequences s
   WHERE ti.pk = 1 AND s.tablename = m.name AND s.seqname = substr(m.name,@PLEN@) || '_' || ti.name || '_seq'
     AND overlite_attidentity(m.sql, ti.name) = '' LIMIT 1))"#;

const PG_ATTRDEF_TMPL: &str = r#"SELECT oid, adrelid, adnum, adbin FROM (
SELECT CAST(m.rowid * 1000 + ti.cid + @OFF@ AS INTEGER) AS oid,
 CAST(m.rowid + @OFF@ AS INTEGER) AS adrelid, ti.cid + 1 AS adnum,
 COALESCE(overlite_default_expr(ti.dflt_value), @SERIAL_DEFAULT@) AS adbin
FROM @MASTER@ m JOIN pragma_table_xinfo(m.name,'@DB@') ti
WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*')
WHERE adbin IS NOT NULL"#;

const PG_INDEX_TMPL: &str = concat!(
    r#"SELECT idx.rowid + @OFF@ AS indexrelid, tbl.rowid + @OFF@ AS indrelid,
 (SELECT count(*) FROM pragma_index_info(idx.name,'@DB@'))
   + COALESCE(json_array_length(overlite_index_include(idx.sql, 1)), 0) AS indnatts,
 (SELECT count(*) FROM pragma_index_info(idx.name,'@DB@')) AS indnkeyatts,
 il."unique" AS indisunique, CASE il.origin WHEN 'pk' THEN 1 ELSE 0 END AS indisprimary,
 0 AS indisexclusion, 1 AS indimmediate, 0 AS indisclustered, 1 AS indisvalid, 0 AS indcheckxmin,
 1 AS indisready, 1 AS indislive, 0 AS indisreplident,
 (SELECT group_concat(COALESCE(NULLIF(ii.cid, -2) + 1, (SELECT t.cid + 1 FROM pragma_table_xinfo(tbl.name,'@DB@') t
     WHERE t.name = overlite_index_null_key(idx.sql, ii.seqno)), 0),' ') FROM pragma_index_info(idx.name,'@DB@') ii)
   || COALESCE((SELECT ' ' || group_concat(n, ' ') FROM (SELECT ti.cid + 1 AS n
        FROM json_each(overlite_index_include(idx.sql, 1)) inc
        JOIN pragma_table_xinfo(tbl.name,'@DB@') ti ON ti.name = inc.value ORDER BY inc.key)), '') AS indkey,
 '' AS indcollation,
 (SELECT group_concat(COALESCE((SELECT oc.oid FROM pg_opclass oc
     WHERE oc.opcname = overlite_index_opclass(idx.sql, ii.seqno) ORDER BY oc.opcmethod <> 403 LIMIT 1), "#,
    index_col_opclass_expr!(),
    r#"),' ') FROM pragma_index_info(idx.name,'@DB@') ii) AS indclass,
 (SELECT group_concat(CASE WHEN x."desc" THEN 3 ELSE 0 END,' ') FROM pragma_index_xinfo(idx.name,'@DB@') x WHERE x.key) AS indoption,
 NULL AS indexprs, NULL AS indpred,
 (SELECT group_concat(COALESCE(x.name, overlite_index_null_key(idx.sql, x.seqno), overlite_index_expr(idx.sql, x.seqno))
   || COALESCE(' ' || overlite_index_opclass(idx.sql, x.seqno), '')
   || CASE WHEN x."desc" THEN ' DESC' ELSE '' END, ', ')
   FROM pragma_index_xinfo(idx.name,'@DB@') x WHERE x.key)
   || COALESCE(') INCLUDE (' || overlite_index_include(idx.sql, 0), '') AS ov_cols,
 overlite_index_predicate(idx.sql) AS ov_pred
FROM @MASTER@ idx
JOIN @MASTER@ tbl ON tbl.name = idx.tbl_name AND tbl.type='table'
JOIN pragma_index_list(tbl.name,'@DB@') il ON il.name = idx.name
WHERE idx.type='index' AND idx.name NOT LIKE 'sqlite_%' AND idx.name NOT GLOB '_overlite_*'
UNION ALL
SELECT tbl.rowid + 90000000 + @OFF@, tbl.rowid + @OFF@,
 (SELECT count(*) FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0),
 (SELECT count(*) FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0),
 1,1,0,1,0,1,0,1,1,0,
 (SELECT group_concat(cid+1,' ') FROM (SELECT cid FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk)),
 '',
 (SELECT group_concat("#,
    pk_col_opclass_expr!(),
    r#",' ') FROM (SELECT cid FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk) pk),
 (SELECT group_concat(0,' ') FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0),
 NULL,NULL,
 (SELECT group_concat(name,', ') FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk), NULL
FROM @MASTER@ tbl
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND EXISTS(SELECT 1 FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0)
UNION ALL
SELECT CAST(tbl.rowid*1000 + il.seq + 84000000 + @OFF@ AS INTEGER), tbl.rowid + @OFF@,
 (SELECT count(*) FROM pragma_index_info(il.name,'@DB@')),
 (SELECT count(*) FROM pragma_index_info(il.name,'@DB@')),
 1,0,0,1,0,1,0,1,1,0,
 (SELECT group_concat(ii.cid+1,' ') FROM pragma_index_info(il.name,'@DB@') ii),
 '',
 (SELECT group_concat("#,
    index_col_opclass_expr!(),
    r#",' ') FROM pragma_index_info(il.name,'@DB@') ii),
 (SELECT group_concat(0,' ') FROM pragma_index_info(il.name,'@DB@') ii),
 NULL,NULL,
 (SELECT group_concat(ii.name,', ') FROM pragma_index_info(il.name,'@DB@') ii), NULL
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1"#
);

const PG_CONSTRAINT_TMPL: &str = concat!(
    r#"SELECT tbl.rowid*100000 + fk.id + @OFF@ AS oid, "#,
    fk_name_expr!(),
    r#" AS conname,
 @NS@ AS connamespace, 'f' AS contype, "#,
    fk_deferral_expr!(),
    r#" > 0 AS condeferrable, "#,
    fk_deferral_expr!(),
    r#" = 2 AS condeferred, 1 AS convalidated,
 CAST(tbl.rowid + @OFF@ AS INTEGER) AS conrelid, 0 AS contypid, 0 AS conindid, 0 AS conparentid,
 CAST((SELECT rowid + @OFF@ FROM @MASTER@ WHERE name = fk."table" AND type='table') AS INTEGER) AS confrelid,
 "#,
    fk_update_action_expr!(),
    r#" AS confupdtype, "#,
    fk_delete_action_expr!(),
    r#" AS confdeltype,
 's' AS confmatchtype, 1 AS conislocal, 0 AS coninhcount, 1 AS connoinherit,
 (SELECT json_group_array((SELECT ti.cid + 1 FROM pragma_table_xinfo(tbl.name,'@DB@') ti WHERE ti.name = k."from"))
    FROM (SELECT "from" FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq) k) AS conkey,
 (SELECT json_group_array((SELECT ti.cid + 1 FROM pragma_table_xinfo(fk."table",'@DB@') ti
                           WHERE ti.name = k."to" OR (k."to" IS NULL AND ti.pk = k.seq + 1)))
    FROM (SELECT "to", seq FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq) k) AS confkey,
 NULL AS conbin,
 (SELECT group_concat(quote_ident("from"), ', ') FROM (SELECT "from" FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq)) AS ov_cols,
 overlite_relation_ref(fk."table", '@FILESCHEMA@') || '(' || (SELECT group_concat(quote_ident(COALESCE(k."to",
     (SELECT ti.name FROM pragma_table_xinfo(fk."table",'@DB@') ti WHERE ti.pk = k.seq + 1))), ', ')
   FROM (SELECT "to", seq FROM pragma_foreign_key_list(tbl.name,'@DB@') WHERE id = fk.id ORDER BY seq) k) || ')' ||
 CASE fk.on_update WHEN 'NO ACTION' THEN '' ELSE ' ON UPDATE ' || fk.on_update END ||
 CASE fk.on_delete WHEN 'NO ACTION' THEN '' ELSE ' ON DELETE ' || fk.on_delete END ||
 CASE "#,
    fk_deferral_expr!(),
    r#" WHEN 2 THEN ' DEFERRABLE INITIALLY DEFERRED' WHEN 1 THEN ' DEFERRABLE' ELSE '' END AS ov_ref
FROM @MASTER@ tbl JOIN pragma_foreign_key_list(tbl.name,'@DB@') fk
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*' AND fk.seq = 0
UNION ALL
SELECT tbl.rowid*100000 + 99999 + @OFF@, "#,
    pk_name_expr!(),
    r#", @NS@, 'p', 0,0,1,
 CAST(tbl.rowid + @OFF@ AS INTEGER), 0, CAST(tbl.rowid + 90000000 + @OFF@ AS INTEGER), 0, 0,
 ' ',' ',' ', 1,0,1,
 (SELECT json_group_array(cid + 1) FROM (SELECT cid FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk)),
 NULL,NULL,
 (SELECT group_concat(name,', ') FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk), NULL
FROM @MASTER@ tbl
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND EXISTS(SELECT 1 FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0)
UNION ALL
-- One row per UNIQUE clause: SQLite makes a single index for two over the same key.
SELECT tbl.rowid*100000 + CASE WHEN un.key = 0 THEN 88000 + il.seq ELSE 89000 + il.seq*10 + un.key END + @OFF@,
 json_extract(un.value, '$.name'), @NS@, 'u', json_extract(un.value, '$.deferral') > 0,
 json_extract(un.value, '$.deferral') = 2, 1,
 CAST(tbl.rowid + @OFF@ AS INTEGER), 0, CAST(tbl.rowid*1000 + il.seq + 84000000 + @OFF@ AS INTEGER), 0, 0,
 ' ',' ',' ', 1,0,1,
 (SELECT json_group_array(ii.cid + 1) FROM pragma_index_info(il.name,'@DB@') ii), NULL, NULL,
 (SELECT group_concat(ii.name,', ') FROM pragma_index_info(il.name,'@DB@') ii), NULL
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
JOIN json_each(overlite_unique_constraints(tbl.sql, substr(tbl.name,@PLEN@),
  (SELECT group_concat(ii.name, ',') FROM pragma_index_info(il.name,'@DB@') ii))) un
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1
UNION ALL
SELECT tbl.rowid*100000 + 77000 + ck.key + @OFF@, json_extract(ck.value, '$.name'), @NS@, 'c', 0,0,
 NOT json_extract(ck.value, '$.notvalid'),
 CAST(tbl.rowid + @OFF@ AS INTEGER), 0, 0, 0, 0, ' ',' ',' ', 1,0,0,
 (SELECT json_group_array(ti.cid + 1) FROM pragma_table_xinfo(tbl.name,'@DB@') ti
   WHERE ti.name IN (SELECT value FROM json_each(json_extract(ck.value, '$.cols')))),
 NULL, NULL, json_extract(ck.value, '$.expr'), NULL
FROM @MASTER@ tbl JOIN json_each(overlite_checks(tbl.sql, substr(tbl.name,@PLEN@))) ck
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  -- An enum column is held as text with a CHECK of its labels, which is no constraint.
  AND NOT (json_extract(ck.value, '$.enum') AND EXISTS (SELECT 1 FROM _overlite_enum_types et
    WHERE (SELECT group_concat(label) FROM (SELECT label FROM _overlite_enums WHERE typname = et.typname ORDER BY sortorder))
          = overlite_enum_labels(tbl.sql, json_extract(ck.value, '$.cols[0]'))))
UNION ALL
-- An EXCLUDE constraint is the pair of triggers ddl/exclude.rs makes; the first carries it.
SELECT x.rid*100000 + 66000 + @OFF@, json_extract(x.j, '$.name'), @NS@, 'x',
 json_extract(x.j, '$.deferrable'), json_extract(x.j, '$.deferred'), 1,
 CAST(tbl.rowid + @OFF@ AS INTEGER), 0, 0, 0, 0, ' ',' ',' ', 1,0,1,
 (SELECT json_group_array(ti.cid + 1) FROM pragma_table_xinfo(tbl.name,'@DB@') ti
   WHERE ti.name IN (SELECT value FROM json_each(json_extract(x.j, '$.cols')))),
 NULL, NULL, json_extract(x.j, '$.def'), NULL
FROM (SELECT trg.rowid AS rid, trg.tbl_name AS tn, overlite_exclusion(trg.sql) AS j FROM @MASTER@ trg
      WHERE trg.type = 'trigger' AND trg.name GLOB '_overlite_excl_i_*') x
JOIN @MASTER@ tbl ON tbl.name = x.tn AND tbl.type = 'table'
WHERE x.j IS NOT NULL"#
);

const INFO_TABLES_TMPL: &str = r#"SELECT @DBNAME@ AS table_catalog, '@PG@' AS table_schema, substr(name,@PLEN@) AS table_name,
 CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type
FROM @MASTER@ WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' AND name NOT GLOB '_overlite_*'"#;

const PG_TABLES_TMPL: &str = r#"SELECT '@PG@' AS schemaname, substr(m.name,@PLEN@) AS tablename,
 @ROLE@ AS tableowner, NULL AS tablespace,
 EXISTS(SELECT 1 FROM @MASTER@ AS i WHERE i.type='index' AND i.tbl_name=m.name) AS hasindexes,
 0 AS hasrules,
 EXISTS(SELECT 1 FROM @MASTER@ AS tg WHERE tg.type='trigger' AND tg.tbl_name=m.name) AS hastriggers,
 0 AS rowsecurity
FROM @MASTER@ AS m
WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'
  AND NOT EXISTS(SELECT 1 FROM @MASTER@ mv WHERE mv.type = 'view' AND mv.name = '_overlite_mv_' || m.name)"#;

// Every index, the primary key's and a UNIQUE constraint's too, with its
// definition as pg_get_indexdef writes it. An index on an expression keeps the SQL
// it was made with.
const PG_INDEXES_TMPL: &str = concat!(
    r#"SELECT '@PG@' AS schemaname, substr(tbl.name,@PLEN@) AS tablename,
 substr(idx.name,@PLEN@) AS indexname, NULL AS tablespace,
 COALESCE('CREATE ' || CASE WHEN il."unique" THEN 'UNIQUE ' ELSE '' END || 'INDEX ' || substr(idx.name,@PLEN@)
   || ' ON @PG@.' || substr(tbl.name,@PLEN@) || ' USING '
   || COALESCE((SELECT am.amname FROM pg_am am WHERE instr(idx.sql, '/*overlite using ' || am.amname || '*/')), 'btree')
   || ' ('
   || (SELECT group_concat(COALESCE(x.name, overlite_index_null_key(idx.sql, x.seqno), overlite_index_expr(idx.sql, x.seqno))
   || COALESCE(' ' || overlite_index_opclass(idx.sql, x.seqno), '')
        || CASE WHEN x."desc" THEN ' DESC' ELSE '' END, ', ')
       FROM pragma_index_xinfo(idx.name,'@DB@') x WHERE x.key) || ')'
   || COALESCE(' INCLUDE (' || overlite_index_include(idx.sql, 0) || ')', '')
   || CASE WHEN instr(idx.sql, '/*overlite nulls not distinct*/') THEN ' NULLS NOT DISTINCT' ELSE '' END
   || COALESCE(' WHERE ' || overlite_index_where(idx.sql), ''), idx.sql) AS indexdef
FROM @MASTER@ idx
JOIN @MASTER@ tbl ON tbl.name = idx.tbl_name AND tbl.type='table'
JOIN pragma_index_list(tbl.name,'@DB@') il ON il.name = idx.name
WHERE idx.type='index' AND idx.name NOT LIKE 'sqlite_%' AND idx.name NOT GLOB '_overlite_*'
 AND idx.sql IS NOT NULL
UNION ALL
SELECT '@PG@', substr(tbl.name,@PLEN@), "#,
    pk_name_expr!(),
    r#", NULL,
 'CREATE UNIQUE INDEX ' || "#,
    pk_name_expr!(),
    r#" || ' ON @PG@.' || substr(tbl.name,@PLEN@) || ' USING btree ('
 || (SELECT group_concat(name, ', ') FROM (SELECT name FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0 ORDER BY pk)) || ')'
FROM @MASTER@ tbl
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND EXISTS(SELECT 1 FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0)
UNION ALL
SELECT '@PG@', substr(tbl.name,@PLEN@),
 "#,
    unique_name_expr!(),
    r#", NULL,
 'CREATE UNIQUE INDEX ' || "#,
    unique_name_expr!(),
    r#"
 || ' ON @PG@.' || substr(tbl.name,@PLEN@) || ' USING btree ('
 || (SELECT group_concat(ii.name, ', ') FROM pragma_index_info(il.name,'@DB@') ii) || ')'
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1"#
);

// A materialized view is a table beside a view `_overlite_mv_<name>` holding its query.
const PG_MATVIEWS_TMPL: &str = r#"SELECT '@PG@' AS schemaname, substr(t.name,@PLEN@) AS matviewname,
 @ROLE@ AS matviewowner, NULL AS tablespace,
 EXISTS(SELECT 1 FROM @MASTER@ i WHERE i.type='index' AND i.tbl_name=t.name) AS hasindexes,
 1 AS ispopulated, substr(v.sql, instr(upper(v.sql), ' AS ') + 4) AS definition
FROM @MASTER@ t JOIN @MASTER@ v ON v.type = 'view' AND v.name = '_overlite_mv_' || t.name
WHERE t.type='table'"#;

const PG_VIEWS_TMPL: &str = r#"SELECT '@PG@' AS schemaname, substr(name,@PLEN@) AS viewname,
 @ROLE@ AS viewowner, overlite_view_body(sql, '@FILESCHEMA@') AS definition
FROM @MASTER@ WHERE type='view' AND name NOT LIKE 'sqlite_%' AND name NOT GLOB '_overlite_*'"#;

const INFO_COLUMNS_TMPL: &str = r#"SELECT @DBNAME@ AS table_catalog, '@PG@' AS table_schema, substr(m.name,@PLEN@) AS table_name,
 ti.name AS column_name, ti.cid + 1 AS ordinal_position,
 COALESCE(overlite_default_expr(ti.dflt_value), @SERIAL_DEFAULT@) AS column_default,
 CASE WHEN ti."notnull"=1 OR ti.pk>0 THEN 'NO' ELSE 'YES' END AS is_nullable,
 CASE WHEN ti.type LIKE '%[]' THEN 'ARRAY'
      ELSE format_type(overlite_type_oid(COALESCE(overlite_identity_type(m.sql, ti.name), ti.type)), NULL) END AS data_type,
 overlite_char_max_length(ti.type) AS character_maximum_length, NULL AS character_octet_length,
 overlite_numeric_precision(ti.type) AS numeric_precision,
 overlite_numeric_precision_radix(ti.type) AS numeric_precision_radix,
 overlite_numeric_scale(ti.type) AS numeric_scale,
 overlite_datetime_precision(ti.type) AS datetime_precision,
 @DBNAME@ AS udt_catalog, 'pg_catalog' AS udt_schema,
 -- udt_name is the pg_type name (int4, timestamp), not the SQL standard
 -- spelling that data_type carries. Clients join information_schema.columns to
 -- pg_type on it, and that join finds nothing when the two are the same.
 COALESCE((SELECT pt.typname FROM pg_type pt
           WHERE pt.oid = overlite_type_oid(COALESCE(overlite_identity_type(m.sql, ti.name), ti.type))),
          format_type(overlite_type_oid(ti.type), NULL)) AS udt_name,
 NULL AS collation_name, NULL AS domain_catalog, NULL AS domain_schema, NULL AS domain_name,
 ti.cid + 1 AS dtd_identifier,
 CASE overlite_attidentity(m.sql, ti.name) WHEN '' THEN 'NO' ELSE 'YES' END AS is_identity, 'NEVER' AS is_generated,
 CASE overlite_attidentity(m.sql, ti.name) WHEN 'a' THEN 'ALWAYS' WHEN 'd' THEN 'BY DEFAULT' END AS identity_generation,
 'YES' AS is_updatable,
 NULL AS identity_start, NULL AS identity_increment, NULL AS identity_maximum,
 NULL AS identity_minimum, NULL AS identity_cycle, NULL AS generation_expression,
 NULL AS interval_type, NULL AS interval_precision, NULL AS scope_catalog,
 NULL AS scope_schema, NULL AS scope_name, NULL AS maximum_cardinality
FROM @MASTER@ m JOIN pragma_table_xinfo(m.name,'@DB@') ti
WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'"#;

const INFO_TABLE_CONSTRAINTS_TMPL: &str = concat!(
    r#"SELECT @DBNAME@ AS constraint_catalog, '@PG@' AS constraint_schema,
 "#,
    pk_name_expr!(),
    r#" AS constraint_name, @DBNAME@ AS table_catalog, '@PG@' AS table_schema,
 tbl.name AS table_name, 'PRIMARY KEY' AS constraint_type, 'NO' AS is_deferrable,
 'NO' AS initially_deferred, 'YES' AS enforced
FROM @MASTER@ tbl
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND EXISTS(SELECT 1 FROM pragma_table_xinfo(tbl.name,'@DB@') WHERE pk>0)
UNION ALL
SELECT 'main', '@PG@', "#,
    fk_name_expr!(),
    r#", 'main', '@PG@', tbl.name,
 'FOREIGN KEY', CASE WHEN "#,
    fk_deferral_expr!(),
    r#" > 0 THEN 'YES' ELSE 'NO' END, CASE WHEN "#,
    fk_deferral_expr!(),
    r#" = 2 THEN 'YES' ELSE 'NO' END, 'YES'
FROM @MASTER@ tbl JOIN pragma_foreign_key_list(tbl.name,'@DB@') fk
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*' AND fk.seq=0
UNION ALL
SELECT 'main', '@PG@',
 "#,
    unique_name_expr!(),
    r#",
 'main', '@PG@', tbl.name, 'UNIQUE', 'NO', 'NO', 'YES'
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1"#
);

const INFO_KEY_COLUMN_USAGE_TMPL: &str = concat!(
    r#"SELECT @DBNAME@ AS constraint_catalog, '@PG@' AS constraint_schema,
 "#,
    pk_name_expr!(),
    r#" AS constraint_name, @DBNAME@ AS table_catalog, '@PG@' AS table_schema,
 tbl.name AS table_name, ti.name AS column_name, ti.pk AS ordinal_position,
 NULL AS position_in_unique_constraint
FROM @MASTER@ tbl JOIN pragma_table_xinfo(tbl.name,'@DB@') ti
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*' AND ti.pk>0
UNION ALL
SELECT 'main', '@PG@', "#,
    fk_name_expr!(),
    r#", 'main', '@PG@', tbl.name,
 fk."from", fk.seq+1, fk.seq+1
FROM @MASTER@ tbl JOIN pragma_foreign_key_list(tbl.name,'@DB@') fk
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
UNION ALL
SELECT 'main', '@PG@',
 "#,
    unique_name_expr!(),
    r#",
 'main', '@PG@', tbl.name, ii.name, ii.seqno+1, NULL
FROM @MASTER@ tbl JOIN pragma_index_list(tbl.name,'@DB@') il
 JOIN pragma_index_info(il.name,'@DB@') ii
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'
  AND il.origin='u' AND il."unique"=1"#
);

const INFO_REFERENTIAL_CONSTRAINTS_TMPL: &str = concat!(
    r#"SELECT @DBNAME@ AS constraint_catalog, '@PG@' AS constraint_schema,
 "#,
    fk_name_expr!(),
    r#" AS constraint_name, @DBNAME@ AS unique_constraint_catalog,
 '@PG@' AS unique_constraint_schema, fk."table" || '_pkey' AS unique_constraint_name,
 'NONE' AS match_option, fk.on_update AS update_rule, fk.on_delete AS delete_rule
FROM @MASTER@ tbl JOIN pragma_foreign_key_list(tbl.name,'@DB@') fk
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*' AND fk.seq=0"#
);

const INFO_CONSTRAINT_COLUMN_USAGE_TMPL: &str = concat!(
    r#"SELECT @DBNAME@ AS table_catalog, '@PG@' AS table_schema,
 tbl.name AS table_name, ti.name AS column_name, @DBNAME@ AS constraint_catalog,
 '@PG@' AS constraint_schema, "#,
    pk_name_expr!(),
    r#" AS constraint_name
FROM @MASTER@ tbl JOIN pragma_table_xinfo(tbl.name,'@DB@') ti
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*' AND ti.pk>0
UNION ALL
SELECT 'main', '@PG@', fk."table", fk."to", 'main', '@PG@', "#,
    fk_name_expr!(),
    r#"
FROM @MASTER@ tbl JOIN pragma_foreign_key_list(tbl.name,'@DB@') fk
WHERE tbl.type='table' AND tbl.name NOT LIKE 'sqlite_%' AND tbl.name NOT GLOB '_overlite_*'"#
);

const INFO_VIEWS_TMPL: &str = r#"SELECT @DBNAME@ AS table_catalog, '@PG@' AS table_schema, name AS table_name,
 sql AS view_definition, 'NONE' AS check_option, 'NO' AS is_updatable, 'NO' AS is_insertable_into,
 'NO' AS is_trigger_updatable, 'NO' AS is_trigger_deletable, 'NO' AS is_trigger_insertable_into
FROM @MASTER@ WHERE type='view' AND name NOT LIKE 'sqlite_%' AND name NOT GLOB '_overlite_*'"#;
