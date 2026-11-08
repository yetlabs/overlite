//! Finding the policy file in a directory.

use crate::error::Result;
use crate::parse::{ConfParser, Parser, YAMLParser};
use crate::policy::Policy;
use std::fs::File;
use std::path::Path;

/// Reads the HBA policy from dir. If pg_hba.conf exists it wins; otherwise
/// pg_hba.yaml (or pg_hba.yml) is used. When none of them exist it returns None,
/// meaning "no HBA configured": the caller then falls back to its default
/// single-method authentication.
///
/// # Errors
///
/// When the file that wins cannot be read or parsed.
pub fn load(dir: &str) -> Result<Option<Policy>> {
    if let Ok(mut f) = File::open(Path::new(dir).join("pg_hba.conf")) {
        return ConfParser.parse(&mut f).map(Some);
    }
    for name in ["pg_hba.yaml", "pg_hba.yml"] {
        if let Ok(mut f) = File::open(Path::new(dir).join(name)) {
            return YAMLParser.parse(&mut f).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::policy::Conn;

    #[test]
    fn load_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let c = Conn {
            ip: "1.2.3.4".parse().ok(),
            database: "d".to_string(),
            user: "u".to_string(),
            ..Default::default()
        };

        // Only YAML present -> YAML is used.
        std::fs::write(
            dir.path().join("pg_hba.yaml"),
            "hba:\n  - {type: host, database: all, user: all, address: 0.0.0.0/0, method: md5}\n",
        )
        .unwrap();
        let p = load(path).unwrap().unwrap();
        assert_eq!(p.method(&c).as_deref(), Some("md5"));

        // Both present -> .conf wins.
        std::fs::write(dir.path().join("pg_hba.conf"), "host all all 0.0.0.0/0 trust\n").unwrap();
        let p = load(path).unwrap().unwrap();
        assert_eq!(
            p.method(&c).as_deref(),
            Some("trust"),
            "pg_hba.conf takes precedence over pg_hba.yaml"
        );

        // Neither present -> no policy (no HBA configured).
        let empty = tempfile::tempdir().unwrap();
        assert!(load(empty.path().to_str().unwrap()).unwrap().is_none());
    }
}
