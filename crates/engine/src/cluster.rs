//! The set of databases one process serves: a directory of `<name>.db` files opened
//! on demand, or a single engine.

use crate::catalog::schema::{filepath_base, is_path_separator};
use crate::db_state::forget_state;
use crate::error::{Error, ErrorKind, Result, errf};
use crate::sqlite::Sqlite;
use overlite_core::sync::MutexExt;
use overlite_core::text::{eq_fold, has_raw_bytes, quoted};
use overlite_core::{SqlState, lazy_regex};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A Cluster is a set of databases served by one process.
pub trait Cluster: Send + Sync + std::fmt::Debug {
    /// The engine backing a database.
    ///
    /// # Errors
    ///
    /// When it is not served, with an error naming what is, or will not open.
    fn engine(&self, database: &str) -> Result<Arc<Sqlite>>;
    /// A caller is done with an engine it took from `engine`.
    fn release(&self, database: &str);
    /// Names served, sorted.
    ///
    /// # Errors
    ///
    /// When the names cannot be listed.
    fn databases(&self) -> Result<Vec<String>>;
    /// Creates a database.
    ///
    /// # Errors
    ///
    /// When the name is invalid or taken, or this cluster cannot hold another.
    fn create_database(&self, database: &str) -> Result<()>;
    /// Drops a database.
    ///
    /// # Errors
    ///
    /// When it is missing, in use, or may not be dropped.
    fn drop_database(&self, database: &str) -> Result<()>;
    /// Gives a database another name.
    ///
    /// # Errors
    ///
    /// When it is missing or in use, or the new name is taken.
    fn rename_database(&self, database: &str, new_name: &str) -> Result<()>;
    /// Closes every database.
    ///
    /// # Errors
    ///
    /// When an engine fails to close.
    fn close(&self) -> Result<()>;
}

// A database is a file, so its name is held to a shape every file system takes. Any
// other name a client can quote is percent-encoded into that shape: `%` is not part
// of it, so the two kinds of file cannot collide.
lazy_regex!(RE_DATABASE_NAME, r"^[A-Za-z_][A-Za-z0-9_-]*$");

/// The longest name Postgres keeps, in bytes.
const MAX_DATABASE_NAME: usize = 63;

/// Whether name can be served as a database: anything Postgres takes as a name.
fn valid_database_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_DATABASE_NAME
}

/// The name of the file, without its `.db`, that holds a database.
fn file_stem(name: &str) -> String {
    if RE_DATABASE_NAME.is_match(name) {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() * 3);
    for (i, b) in name.bytes().enumerate() {
        if b.is_ascii_alphabetic() || b == b'_' || (i > 0 && (b.is_ascii_digit() || b == b'-')) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The database a file's stem holds, None for a file this server did not write.
fn database_of_file(stem: &str) -> Option<String> {
    let name = if stem.contains('%') {
        let (b, mut out) = (stem.as_bytes(), Vec::with_capacity(stem.len()));
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                let hex = stem.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()?
    } else {
        stem.to_string()
    };
    (valid_database_name(&name) && file_stem(&name) == stem).then_some(name)
}

/// Normalizes a path: no `.`, no `a/..`, no repeated or trailing separator. The result
/// separates with `/`, which Windows takes as well.
fn filepath_clean(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let rooted = p.starts_with(is_path_separator);
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split(is_path_separator) {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&l) if l != ".." => {
                    out.pop();
                }
                _ => {
                    if !rooted {
                        out.push("..");
                    }
                }
            },
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    // `C:` alone is the current directory of drive C, not its root.
    if cfg!(windows) && !rooted && out.len() == 1 && joined.len() == 2 && joined.ends_with(':') {
        return format!("{joined}/");
    }
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// The directory of a path.
fn filepath_dir(p: &str) -> String {
    match p.rfind(is_path_separator) {
        Some(i) => filepath_clean(&p[..=i]),
        None => ".".to_string(),
    }
}

/// Joins dir and a non-empty file name.
fn filepath_join(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        return filepath_clean(file);
    }
    filepath_clean(&format!("{dir}/{file}"))
}

/// An os error in the form `<op> <path>: no such file or directory`.
fn path_error(op: &str, path: &str, e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = match text.find(" (os error") {
        Some(i) => &text[..i],
        None => text.as_str(),
    };
    let mut chars = text.chars();
    let text = match chars.next() {
        Some(c) => format!("{}{}", c.to_lowercase(), chars.as_str()),
        None => String::new(),
    };
    format!("{op} {path}: {text}")
}

// --- one database -------------------------------------------------------------

/// Serves exactly one database, the file overlite was pointed at.
#[derive(Debug)]
struct Single {
    name: String,
    eng: Arc<Sqlite>,
}

/// Wraps one engine as a cluster of one.
pub fn single(name: &str, eng: Arc<Sqlite>) -> Arc<dyn Cluster> {
    Arc::new(Single {
        name: name.to_string(),
        eng,
    })
}

impl Cluster for Single {
    fn engine(&self, database: &str) -> Result<Arc<Sqlite>> {
        // An empty name means the client did not ask for one.
        if database.is_empty() || eq_fold(database, &self.name) {
            return Ok(self.eng.clone());
        }
        Err(NoSuchDatabaseError {
            name: database.to_string(),
            available: vec![self.name.clone()],
        }
        .into())
    }

    fn release(&self, _database: &str) {}

    fn databases(&self) -> Result<Vec<String>> {
        Ok(vec![self.name.clone()])
    }

    fn create_database(&self, _database: &str) -> Result<()> {
        Err(Error::with_state(SqlState::FEATURE_NOT_SUPPORTED, ERR_NOT_A_DIRECTORY))
    }

    fn drop_database(&self, _database: &str) -> Result<()> {
        Err(Error::with_state(SqlState::FEATURE_NOT_SUPPORTED, ERR_NOT_A_DIRECTORY))
    }

    fn rename_database(&self, _database: &str, _new_name: &str) -> Result<()> {
        Err(Error::with_state(SqlState::FEATURE_NOT_SUPPORTED, ERR_NOT_A_DIRECTORY))
    }

    fn close(&self) -> Result<()> {
        self.eng.close()
    }
}

const ERR_NOT_A_DIRECTORY: &str = "this server holds a single database; point overlite at a file in a writable directory to create and drop databases";

// --- a directory of databases -------------------------------------------------

/// Serves every <name>.db beside the file overlite was pointed at, opening each
/// on demand. Idle engines are closed once max_open is exceeded.
#[derive(Debug)]
struct Dir {
    dir: String,
    /// The database named by the path overlite was started with. It is always
    /// there to connect to, so it cannot be dropped.
    entry: String,
    max_open: usize,
    mu: Mutex<DirState>,
}

/// What Dir's mutex protects.
#[derive(Debug)]
struct DirState {
    open: HashMap<String, OpenDB>,
    seq: u64, // bumps on use, to find the least recently used
}

#[derive(Debug)]
struct OpenDB {
    eng: Arc<Sqlite>,
    refs: i64, // connections currently on it
    used: u64, // seq at last use
}

/// How many engines a Dir keeps open.
pub const DEFAULT_MAX_OPEN_DATABASES: usize = 64;

/// Always exists, as it does in PostgreSQL: a client has to be connected to some
/// database before it can create one, and every tool defaults to this name.
const MAINTENANCE_DATABASE: &str = "postgres";

/// Serves the database at path, and every `<name>.db` beside it. The file at path
/// is created if it does not exist; so is the maintenance database.
///
/// # Errors
///
/// When the directory cannot be made or read, or the file's name cannot be a database.
pub fn open_cluster(path: &str, max_open: usize) -> Result<Arc<dyn Cluster>> {
    let max_open = if max_open == 0 {
        DEFAULT_MAX_OPEN_DATABASES
    } else {
        max_open
    };
    let dir = filepath_dir(path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(errf!(
            SqlState::IO_ERROR,
            "create database directory {}: {}",
            dir,
            path_error("mkdir", &dir, &e)
        ));
    }

    let base = filepath_base(path);
    let entry = base.strip_suffix(".db").unwrap_or(&base).to_string();
    if !RE_DATABASE_NAME.is_match(&entry) || entry.len() > MAX_DATABASE_NAME {
        return Err(errf!(
            SqlState::INVALID_NAME,
            "{entry} is not a usable database name: a database name must start with a letter or underscore and contain only letters, digits, underscores and dashes, because it is the name of the file that stores it"
        ));
    }

    let d = Dir {
        dir,
        entry: entry.clone(),
        max_open,
        mu: Mutex::new(DirState {
            open: HashMap::new(),
            seq: 0,
        }),
    };
    // The entry database and the maintenance one. They are usually different,
    // and when path already names the maintenance database this creates it once.
    for name in [entry.as_str(), MAINTENANCE_DATABASE] {
        if d.occupied(name) {
            continue;
        }
        d.create_database(name)?;
    }
    // Every database here in the storage format before the first client, so that one
    // that cannot be (its backup cannot be written) stops the server at its start
    // rather than at its first connection.
    if let Ok(entries) = std::fs::read_dir(&d.dir) {
        let mut files: Vec<String> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                n.strip_suffix(".db")
                    .is_some_and(|stem| RE_DATABASE_NAME.is_match(stem))
            })
            .collect();
        files.sort();
        for f in files {
            let path = filepath_join(&d.dir, &f);
            crate::storage::ensure_storage_format(&path).map_err(|e| crate::storage::conversion_error(&path, &e))?;
        }
    }
    Ok(Arc::new(d))
}

/// How long a DROP DATABASE waits for the database's last clients to go, as
/// Postgres waits for their backends to exit.
const DROP_WAIT: Duration = Duration::from_secs(5);

impl Dir {
    fn path(&self, name: &str) -> String {
        filepath_join(&self.dir, &format!("{}.db", file_stem(name)))
    }

    /// Whether the database name is here: its file, by that exact name. A filesystem
    /// that does not tell case apart (macOS's APFS by default) finds `Shop.db` for
    /// `shop.db` too, so a file found is also looked for among the directory's names.
    fn exists(&self, name: &str) -> bool {
        self.occupied(name) && {
            let file = format!("{}.db", file_stem(name));
            std::fs::read_dir(&self.dir).is_ok_and(|rd| {
                rd.filter_map(std::result::Result::ok)
                    .any(|e| e.file_name().to_str() == Some(file.as_str()))
            })
        }
    }

    /// Whether the file name would open is taken: by database name, or by another
    /// whose name differs only in case on a filesystem that does not tell case apart.
    fn occupied(&self, name: &str) -> bool {
        std::fs::metadata(self.path(name)).is_ok()
    }

    /// Closes idle engines until at most max_open are open. An engine with a
    /// connection on it is never closed.
    fn evict_locked(&self, d: &mut DirState) {
        while d.open.len() > self.max_open {
            let mut victim: Option<&String> = None;
            let mut oldest = 0u64;
            for (name, e) in &d.open {
                if e.refs > 0 {
                    continue;
                }
                if victim.is_none() || e.used < oldest {
                    victim = Some(name);
                    oldest = e.used;
                }
            }
            let Some(victim) = victim.cloned() else {
                return; // everything open is in use
            };
            let _ = d.open[&victim].eng.close();
            d.open.remove(&victim);
        }
    }

    /// Takes the mutex once nobody holds database open, waiting up to DROP_WAIT
    /// for the holders to leave. A client that has just disconnected is still
    /// counted until its connection is torn down.
    fn lock_idle(&self, database: &str) -> Result<MutexGuard<'_, DirState>> {
        let deadline = Instant::now() + DROP_WAIT;
        loop {
            let d = self.mu.locked();
            match d.open.get(database) {
                Some(e) if e.refs != 0 => {}
                _ => return Ok(d),
            }
            drop(d);
            if Instant::now() > deadline {
                return Err(Error::with_state(
                    SqlState::OBJECT_IN_USE,
                    format!("database {:?} is being accessed by other users", quoted(database)),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The error for a name whose file is taken: the database itself, or one whose
    /// name differs only in case, where the filesystem does not tell case apart.
    fn taken(&self, name: &str) -> Error {
        if self.exists(name) {
            return duplicate_database(name);
        }
        errf!(
            SqlState::DUPLICATE_DATABASE,
            "database {:?} cannot be created: a database whose name differs only in case holds its file, and this filesystem does not tell case apart",
            quoted(name)
        )
    }

    /// Lists a few names to put in a "no such database" message.
    fn available_for_error(&self) -> Vec<String> {
        let Ok(mut names) = self.databases() else {
            return Vec::new();
        };
        const MAX: usize = 5;
        if names.len() > MAX {
            names.truncate(MAX);
            names.push("…".to_string());
        }
        names
    }
}

impl Cluster for Dir {
    /// Lists the files in the directory.
    fn databases(&self) -> Result<Vec<String>> {
        let rd = std::fs::read_dir(&self.dir).map_err(|e| {
            errf!(
                SqlState::IO_ERROR,
                "read database directory {}: {}",
                self.dir,
                path_error("open", &self.dir, &e)
            )
        })?;
        let mut names = Vec::new();
        for e in rd {
            let e = e.map_err(|e| {
                errf!(
                    SqlState::IO_ERROR,
                    "read database directory {}: {}",
                    self.dir,
                    path_error("readdirent", &self.dir, &e)
                )
            })?;
            let file = e.file_name().to_string_lossy().into_owned();
            #[allow(clippy::case_sensitive_file_extension_comparisons)] // a database is exactly <name>.db
            if e.file_type().is_ok_and(|t| t.is_dir()) || !file.ends_with(".db") {
                continue;
            }
            names.extend(database_of_file(&file[..file.len() - ".db".len()]));
        }
        names.sort();
        Ok(names)
    }

    /// Opens a database, or returns it if it is already open. The caller must
    /// release it when done.
    fn engine(&self, database: &str) -> Result<Arc<Sqlite>> {
        if database.is_empty() {
            return Err(NoSuchDatabaseError {
                name: database.to_string(),
                available: self.available_for_error(),
            }
            .into());
        }
        if !valid_database_name(database) || !self.exists(database) {
            return Err(NoSuchDatabaseError {
                name: database.to_string(),
                available: self.available_for_error(),
            }
            .into());
        }

        let mut guard = self.mu.locked();
        let d = &mut *guard;
        if let Some(e) = d.open.get_mut(database) {
            e.refs += 1;
            d.seq += 1;
            e.used = d.seq;
            return Ok(e.eng.clone());
        }

        let eng = Sqlite::open(&self.path(database))?;
        d.seq += 1;
        d.open.insert(
            database.to_string(),
            OpenDB {
                eng: eng.clone(),
                refs: 1,
                used: d.seq,
            },
        );
        self.evict_locked(d);
        Ok(eng)
    }

    /// Drops a caller's hold, making the database eligible to be closed.
    fn release(&self, database: &str) {
        let mut guard = self.mu.locked();
        let d = &mut *guard;
        if let Some(e) = d.open.get_mut(database)
            && e.refs > 0
        {
            e.refs -= 1;
        }
        self.evict_locked(d);
    }

    /// Creates the file and its catalog.
    fn create_database(&self, database: &str) -> Result<()> {
        check_database_name(database)?;
        if self.occupied(database) {
            return Err(self.taken(database));
        }

        let mut guard = self.mu.locked();
        let d = &mut *guard;
        let eng = Sqlite::open(&self.path(database)).map_err(|e| {
            errf!(
                SqlState::IO_ERROR,
                "create database {database:?}: {e}",
                database = quoted(database)
            )
        })?;
        d.seq += 1;
        d.open.insert(
            database.to_string(),
            OpenDB {
                eng,
                refs: 0,
                used: d.seq,
            },
        );
        self.evict_locked(d);
        Ok(())
    }

    /// Closes the database and deletes its files.
    fn drop_database(&self, database: &str) -> Result<()> {
        if database == MAINTENANCE_DATABASE {
            return Err(Error::with_state(
                SqlState::OBJECT_IN_USE,
                format!(
                    "cannot drop the maintenance database {:?}",
                    quoted(MAINTENANCE_DATABASE)
                ),
            ));
        }
        if database == self.entry {
            return Err(Error::with_state(
                SqlState::OBJECT_IN_USE,
                format!(
                    "cannot drop {:?}: it is the database this server was started with, and the one a client connects to before it can create another",
                    quoted(database)
                ),
            ));
        }
        if !self.exists(database) {
            return Err(undefined_database(database));
        }

        let mut d = self.lock_idle(database)?;
        if let Some(e) = d.open.get(database) {
            let _ = e.eng.close();
            d.open.remove(database);
        }
        let path = self.path(database);
        forget_state(&path);
        for p in [path.clone(), format!("{path}-wal"), format!("{path}-shm")] {
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(errf!(
                    SqlState::IO_ERROR,
                    "drop database {database:?}: {}",
                    path_error("remove", &p, &e),
                    database = quoted(database)
                ));
            }
        }
        Ok(())
    }

    /// Closes the database and moves its file. A closed database has no -wal or
    /// -shm beside it: closing is what checkpoints and removes them.
    fn rename_database(&self, database: &str, new_name: &str) -> Result<()> {
        if database == MAINTENANCE_DATABASE || database == self.entry {
            return Err(Error::with_state(
                SqlState::OBJECT_IN_USE,
                format!(
                    "database {:?} is always there to connect to, and cannot be renamed",
                    quoted(database)
                ),
            ));
        }
        if !self.exists(database) {
            return Err(undefined_database(database));
        }
        check_database_name(new_name)?;
        if self.occupied(new_name) {
            return Err(self.taken(new_name));
        }
        let mut d = self.lock_idle(database)?;
        if let Some(e) = d.open.remove(database) {
            let _ = e.eng.close();
        }
        let (from, to) = (self.path(database), self.path(new_name));
        forget_state(&from);
        std::fs::rename(&from, &to).map_err(|e| {
            errf!(
                SqlState::IO_ERROR,
                "rename database {database:?}: {}",
                path_error("rename", &from, &e),
                database = quoted(database)
            )
        })
    }

    fn close(&self) -> Result<()> {
        let mut d = self.mu.locked();
        for (_, e) in d.open.drain() {
            let _ = e.eng.close();
        }
        Ok(())
    }
}

fn check_database_name(name: &str) -> Result<()> {
    if has_raw_bytes(name.as_bytes()) {
        return Err(Error::with_state(
            SqlState::CHARACTER_NOT_IN_REPERTOIRE,
            format!("database name {name:?} is not UTF-8"),
        ));
    }
    if valid_database_name(name) {
        return Ok(());
    }
    Err(Error::with_state(
        SqlState::NAME_TOO_LONG,
        format!("database name \"{name}\" is longer than {MAX_DATABASE_NAME} bytes"),
    ))
}

fn duplicate_database(name: &str) -> Error {
    Error::with_state(
        SqlState::DUPLICATE_DATABASE,
        format!("database \"{name}\" already exists"),
    )
}

fn undefined_database(name: &str) -> Error {
    Error::with_state(
        SqlState::INVALID_CATALOG_NAME,
        format!("database \"{name}\" does not exist"),
    )
}

/// A client asked for a database this server does not serve. It carries what is
/// available, because the usual cause is pointing at the wrong server or
/// misspelling the name.
#[derive(Debug)]
struct NoSuchDatabaseError {
    name: String,
    available: Vec<String>,
}

impl NoSuchDatabaseError {
    fn error(&self) -> String {
        if self.available.is_empty() {
            return format!("database {:?} does not exist", quoted(&self.name));
        }
        format!(
            "database {:?} does not exist; this server serves {}",
            quoted(&self.name),
            self.available.join(", ")
        )
    }
}

impl From<NoSuchDatabaseError> for Error {
    fn from(e: NoSuchDatabaseError) -> Self {
        Error::new(ErrorKind::NoSuchDatabase, e.error())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sqlite::tests::TempDir;

    fn has(names: &[String], name: &str) -> bool {
        names.iter().any(|n| n == name)
    }

    // The maintenance database has to be there whatever the server was pointed
    // at. It was added once and then lost when the directory started being taken
    // from the file, because nothing asserted it: this is that assertion.
    #[test]
    fn maintenance_database_always_exists() {
        let dir = TempDir::new();

        let cluster = open_cluster(&dir.join("shop.db"), 4).unwrap();

        let names = cluster.databases().unwrap();
        assert!(
            has(&names, MAINTENANCE_DATABASE),
            "databases = {names:?}, want {MAINTENANCE_DATABASE:?} among them"
        );
        assert!(
            has(&names, "shop"),
            "databases = {names:?}, want the entry database too"
        );

        // It has to be connectable, not merely listed: the point is that a client
        // with nowhere else to go can land on it and run CREATE DATABASE.
        cluster.engine(MAINTENANCE_DATABASE).unwrap();
        cluster.release(MAINTENANCE_DATABASE);
        cluster.close().unwrap();
    }

    // Pointing overlite at the maintenance database itself must not trip over
    // creating it twice.
    #[test]
    fn maintenance_database_as_the_entry() {
        let dir = TempDir::new();

        let cluster = open_cluster(&dir.join("postgres.db"), 4).unwrap();

        let names = cluster.databases().unwrap();
        assert_eq!(names, vec![MAINTENANCE_DATABASE.to_string()]);
        cluster.close().unwrap();
    }

    // An existing directory gains the maintenance database without disturbing what
    // is already there, which is what an upgrade looks like.
    #[test]
    fn maintenance_database_is_added_to_an_existing_directory() {
        let dir = TempDir::new();
        for name in ["acme", "globex"] {
            std::fs::write(dir.join(&format!("{name}.db")), b"").unwrap();
        }

        let cluster = open_cluster(&dir.join("acme.db"), 4).unwrap();

        let names = cluster.databases().unwrap();
        for want in ["acme", "globex", MAINTENANCE_DATABASE] {
            assert!(has(&names, want), "databases = {names:?}, want {want:?} among them");
        }
        cluster.close().unwrap();
    }

    #[test]
    fn database_file_names() {
        for (name, stem) in [
            ("shop", "shop"),
            ("a-b_1", "a-b_1"),
            ("a b", "a%20b"),
            ("../up", "%2E%2E%2Fup"),
            ("1a", "%31a"),
            ("-a", "%2Da"),
            ("a.b", "a%2Eb"),
            ("100%", "%3100%25"),
            ("ação", "a%C3%A7%C3%A3o"),
        ] {
            assert_eq!(file_stem(name), stem);
            assert_eq!(database_of_file(stem).as_deref(), Some(name));
        }
        for stem in ["a%2", "a%zz", "a%41", "%FF", "a.b", ""] {
            assert_eq!(database_of_file(stem), None, "{stem}");
        }
    }

    // Dropping it would put the server back in the state this guarantee exists to
    // prevent.
    #[test]
    fn maintenance_database_cannot_be_dropped() {
        let dir = TempDir::new();
        let cluster = open_cluster(&dir.join("shop.db"), 4).unwrap();

        assert!(
            cluster.drop_database(MAINTENANCE_DATABASE).is_err(),
            "drop_database on the maintenance database succeeded"
        );
        cluster.close().unwrap();
    }
}
