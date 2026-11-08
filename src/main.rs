//! overlite: speaks the PostgreSQL wire protocol on the front and stores
//! everything in SQLite files on the back.

mod logger;

use clap::Parser;
use overlite_core::text::quoted;
use overlite_engine::{self as engine, Cluster};
use overlite_postgres::{Protocol, Server};
use std::path::Path;
use std::sync::Arc;

/// Why the server could not run; the text goes to the log before exiting.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The release tag when CI sets one.
const VERSION: &str = match option_env!("OVERLITE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// A flag given twice takes the last, as the Go server's did: the image's ENTRYPOINT
/// passes `--host 0.0.0.0`, and a compose file may pass it again.
#[derive(Parser)]
#[command(
    name = "overlite",
    version = VERSION,
    about = "A PostgreSQL-speaking server backed by SQLite",
    args_override_self = true
)]
struct Args {
    /// Path to the SQLite file; a positional wins over --db
    db_file: Option<String>,
    /// Wire protocol to speak
    #[arg(long, env = "OVERLITE_DRIVER", default_value = "postgres")]
    driver: String,
    /// Listen address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[allow(rustdoc::invalid_html_tags)] // clap prints this text, so it stays plain
    /// Port to listen on (default: the protocol's, or <DRIVER>_PORT)
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Path to the SQLite file (or :memory:); its name becomes the database name
    #[arg(long, default_value = "postgres.db")]
    db: String,
    /// How many databases to keep open at once; the rest reopen on demand
    #[arg(long = "max-open-databases", default_value_t = engine::DEFAULT_MAX_OPEN_DATABASES)]
    max_open_databases: usize,
}

fn main() {
    logger::init();
    let args = Args::parse();
    let db = args.db_file.clone().unwrap_or_else(|| args.db.clone());
    if let Err(e) = run(&args.driver, &args.host, &db, args.port, args.max_open_databases) {
        log::error!("{e}");
        std::process::exit(1);
    }
}

fn run(driver: &str, host: &str, db: &str, port: u16, max_dbs: usize) -> Result<()> {
    if driver != "postgres" {
        return Err(format!("unknown driver {:?} (supported: postgres)", quoted(driver)).into());
    }
    let proto = Protocol::from_env();
    let (cluster, describe) = open_cluster(db, max_dbs)?;

    // --port wins; otherwise the driver's default, overridable via <DRIVER>_PORT.
    let port = if port != 0 {
        port
    } else {
        env_int(&format!("{}_PORT", driver.to_uppercase()), proto.default_port())
    };
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut srv = Server::new(&addr, proto, cluster.clone()).map_err(|e| format!("listen: {e}"))?;

    // SIGINT and SIGTERM, or Ctrl-C on Windows. The handler runs on its own
    // thread, not in signal context.
    let stopper = srv.stopper()?;
    ctrlc::set_handler(move || stopper.stop()).map_err(|e| format!("signal handler: {e}"))?;

    log::info!(
        "overlite: {} protocol on {} -> {} (user={} auth={} tls={})",
        driver,
        srv.addr(),
        describe,
        current_user(),
        auth_mode(),
        tls_mode()
    );
    let served = srv.serve();
    srv.drain();
    cluster.close()?;
    Ok(served?)
}

fn db_name(path: &str) -> String {
    let base = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".db").map(str::to_string).unwrap_or(base)
}

fn current_user() -> String {
    match std::env::var("POSTGRES_USER") {
        Ok(u) if !u.is_empty() => u,
        _ => "postgres".to_string(),
    }
}

fn auth_mode() -> String {
    let dir = std::env::var("OVERLITE_HBA_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| ".".into());
    for name in ["pg_hba.conf", "pg_hba.yaml", "pg_hba.yml"] {
        if Path::new(&dir).join(name).exists() {
            return format!("pg_hba ({name})");
        }
    }
    if std::env::var("POSTGRES_PASSWORD").unwrap_or_default().is_empty() {
        return "trust".to_string();
    }
    match std::env::var("POSTGRES_HOST_AUTH_METHOD")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "trust" => "trust".into(),
        "password" => "password (cleartext)".into(),
        "md5" => "md5".into(),
        _ => "scram-sha-256".into(),
    }
}

fn tls_mode() -> &'static str {
    let set = |k: &str| !std::env::var(k).unwrap_or_default().is_empty();
    if (set("POSTGRES_SSL_CERT") && set("POSTGRES_SSL_KEY")) || set("POSTGRES_SSL") {
        "on"
    } else {
        "off"
    }
}

fn env_int(key: &str, def: u16) -> u16 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(def)
}

/// Explains the common ways --db is wrong.
fn describe_db_path(db: &str, err: &engine::Error) -> String {
    if db.is_empty() || db == ":memory:" {
        return err.to_string();
    }
    let p = Path::new(db);
    if p.is_dir() {
        return format!(
            "{db} is a directory; --db takes the path to a SQLite file, e.g. --db {}: {err}",
            p.join("postgres.db").display()
        );
    }
    if let Some(dir) = p.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        return format!("the directory {} does not exist: {err}", dir.display());
    }
    err.to_string()
}

/// Builds the set of databases to serve. A file on disk brings its directory
/// with it: every `<name>.db` beside it is a database too.
fn open_cluster(db: &str, max_dbs: usize) -> Result<(Arc<dyn Cluster>, String)> {
    if db == ":memory:" {
        let eng = engine::Sqlite::open(db)?;
        return Ok((engine::single("main", eng), "sqlite :memory: (db=main)".to_string()));
    }
    let cluster = engine::open_cluster(db, max_dbs).map_err(|e| format!("open {db}: {}", describe_db_path(db, &e)))?;
    let names = cluster.databases().unwrap_or_default();
    let dir = Path::new(db)
        .parent()
        .map(|d| d.display().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| ".".into());
    Ok((
        cluster,
        format!(
            "sqlite {db} (db={}, {} in {dir}, {max_dbs} kept open)",
            db_name(db),
            names.len()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_flag_takes_the_last() {
        let a = Args::try_parse_from(["overlite", "--host", "0.0.0.0", "--host", "127.0.0.1", "x.db"]).unwrap();
        assert_eq!((a.host.as_str(), a.db_file.as_deref()), ("127.0.0.1", Some("x.db")));
    }
}
