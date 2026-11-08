//! PostgreSQL-style host-based authentication: an ordered set of rules that maps
//! an incoming connection (type, database, user, address) to an authentication
//! method (or a rejection). Two on-disk formats are supported through the Parser
//! trait, the classic pg_hba.conf text format and a YAML equivalent, and `load`
//! picks between them (see load.rs).

use overlite_core::text::eq_fold;
use std::collections::HashMap;
use std::net::IpAddr;

/// An incoming connection to be authorized.
#[derive(Clone, Debug, Default)]
pub struct Conn {
    /// Unix-socket connection (vs TCP)
    pub local: bool,
    /// The connection was upgraded to TLS
    pub ssl: bool,
    /// Requested database
    pub database: String,
    /// Requested role
    pub user: String,
    /// Client address (None for local connections)
    pub ip: Option<IpAddr>,
}

/// 4-byte network and mask for IPv4, 16-byte for IPv6.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IpNet {
    ip: Vec<u8>,
    mask: Vec<u8>,
}

/// The 4 bytes of an IPv4 address, mapped or not, or the 16 of any other.
fn ip_bytes(ip: IpAddr) -> Vec<u8> {
    match ip.to_canonical() {
        IpAddr::V4(a) => a.octets().to_vec(),
        IpAddr::V6(a) => a.octets().to_vec(),
    }
}

impl IpNet {
    /// Parses a CIDR, keeping only the network.
    fn parse_cidr(s: &str) -> Option<IpNet> {
        let (addr, mask) = s.split_once('/')?;
        let ip: IpAddr = addr.parse().ok()?;
        if mask.is_empty() || !mask.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n: usize = mask.parse().ok()?;
        let octets = match ip {
            IpAddr::V4(a) => a.octets().to_vec(),
            IpAddr::V6(a) => a.octets().to_vec(),
        };
        if n > octets.len() * 8 {
            return None;
        }
        let mask: Vec<u8> = (0..octets.len())
            .map(|i| {
                let bits = n.saturating_sub(i * 8).min(8);
                if bits == 0 { 0 } else { 0xffu8 << (8 - bits) }
            })
            .collect();
        let ip = octets.iter().zip(&mask).map(|(a, m)| a & m).collect();
        Some(IpNet { ip, mask })
    }

    /// Whether the network holds the address.
    fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4-mapped network compares as IPv4.
        let (nn, m): (&[u8], &[u8]) = if self.ip.len() == 16
            && self.ip[..10].iter().all(|b| *b == 0)
            && self.ip[10] == 0xff
            && self.ip[11] == 0xff
        {
            (&self.ip[12..], &self.mask[12..])
        } else {
            (&self.ip, &self.mask)
        };
        let x = ip_bytes(ip);
        if x.len() != nn.len() {
            return false;
        }
        (0..x.len()).all(|i| nn[i] & m[i] == x[i] & m[i])
    }
}

/// One HBA entry: a match on type/database/user/address that yields an
/// authentication method.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rule {
    /// local | host | hostssl | hostnossl
    pub(crate) typ: String,
    /// all | name | csv | replication | sameuser | ...
    pub(crate) database: String,
    /// all | name | csv | +group
    pub(crate) user: String,
    /// "" (local) | all | samehost | CIDR
    pub(crate) address: String,
    /// trust | reject | scram-sha-256 | md5 | password | peer | cert | ...
    pub(crate) method: String,
    /// Trailing key=value settings
    pub(crate) options: HashMap<String, String>,
    /// Parsed address when it is a CIDR
    pub(crate) cidr: Option<IpNet>,
}

/// An ordered rule set.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub(crate) rules: Vec<Rule>,
}

impl Policy {
    /// The authentication method of the first rule that matches c. None when no
    /// rule matches: Postgres rejects such a connection.
    pub fn method(&self, c: &Conn) -> Option<String> {
        for r in &self.rules {
            if r.matches(c) {
                return Some(r.method.clone());
            }
        }
        None
    }
}

impl Rule {
    fn matches(&self, c: &Conn) -> bool {
        self.match_type(c)
            && match_database(&self.database, &c.database)
            && match_user(&self.user, &c.user)
            && self.match_addr(c)
    }

    fn match_type(&self, c: &Conn) -> bool {
        match self.typ.to_lowercase().as_str() {
            "local" => c.local,
            "host" => !c.local,
            "hostssl" => !c.local && c.ssl,
            "hostnossl" => !c.local && !c.ssl,
            _ => false,
        }
    }

    fn match_addr(&self, c: &Conn) -> bool {
        if c.local {
            return true; // a local rule has no address column
        }
        match self.address.trim().to_lowercase().as_str() {
            "all" => return true,
            "samehost" => return c.ip.is_some_and(|ip| ip.to_canonical().is_loopback()),
            "" => return false,
            _ => {}
        }
        if let Some(cidr) = &self.cidr {
            return c.ip.is_some_and(|ip| cidr.contains(ip));
        }
        false // hostnames aren't resolved
    }

    /// Parses the address into a CIDR when applicable.
    pub(crate) fn compile(&mut self) {
        if let Some(ipnet) = IpNet::parse_cidr(&self.address) {
            self.cidr = Some(ipnet);
        }
    }
}

/// Matches an HBA database field (all / csv list / name). The pseudo-values that
/// need connection context we don't have (replication, sameuser, ...) are treated
/// as non-matching.
fn match_database(pattern: &str, db: &str) -> bool {
    for p in pattern.split(',') {
        let p = p.trim();
        match p.to_lowercase().as_str() {
            "all" => return true,
            "replication" | "sameuser" | "samerole" | "samegroup" => {}
            _ => {
                if p == db {
                    return true;
                }
            }
        }
    }
    false
}

/// Matches an HBA user field (all / csv list / name / +group). Group membership
/// isn't modeled, so "+ops" matches a user literally named "ops".
fn match_user(pattern: &str, user: &str) -> bool {
    for p in pattern.split(',') {
        let p = p.trim();
        if eq_fold(p, "all") {
            return true;
        } else if let Some(g) = p.strip_prefix('+') {
            if eq_fold(g, user) {
                return true;
            }
        } else if p == user {
            return true;
        }
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::parse::{ConfParser, Parser};

    fn conn(ip: &str, database: &str, user: &str) -> Conn {
        Conn {
            ip: ip.parse().ok(),
            database: database.to_string(),
            user: user.to_string(),
            ..Default::default()
        }
    }

    fn conf(text: &str) -> Policy {
        ConfParser.parse(&mut text.as_bytes()).unwrap()
    }

    #[test]
    fn match_first_wins() {
        let p = conf(
            "
host    all    all      127.0.0.1/32    trust
host    shop   app      10.0.0.0/8      md5
hostssl all    all      0.0.0.0/0       scram-sha-256
host    all    all      0.0.0.0/0       reject
",
        );

        // Loopback -> trust (first rule).
        assert_eq!(p.method(&conn("127.0.0.1", "x", "y")).as_deref(), Some("trust"));

        // app@shop from 10.x -> md5.
        assert_eq!(p.method(&conn("10.1.2.3", "shop", "app")).as_deref(), Some("md5"));

        // bob@shop from 10.x without TLS: md5 rule needs user 'app', hostssl needs
        // TLS, so it falls through to the final reject.
        assert_eq!(
            p.method(&Conn {
                ssl: false,
                ..conn("10.1.2.3", "shop", "bob")
            })
            .as_deref(),
            Some("reject")
        );

        // Same but over TLS -> the hostssl scram rule matches.
        assert_eq!(
            p.method(&Conn {
                ssl: true,
                ..conn("10.1.2.3", "shop", "bob")
            })
            .as_deref(),
            Some("scram-sha-256")
        );
    }

    #[test]
    fn no_match() {
        let p = conf("host all all 127.0.0.1/32 trust\n");
        assert_eq!(
            p.method(&conn("8.8.8.8", "x", "y")),
            None,
            "an unmatched connection reports no rule"
        );
    }

    #[test]
    fn local_and_group_and_csv() {
        let p = conf(
            "
local   all              all                       peer
host    db1,db2          +ops     10.0.0.0/8       md5
",
        );

        let local = Conn {
            local: true,
            database: "anything".to_string(),
            user: "root".to_string(),
            ..Default::default()
        };
        assert_eq!(p.method(&local).as_deref(), Some("peer"));

        // db in the CSV list + member of +ops (name match) + in CIDR.
        assert_eq!(p.method(&conn("10.9.9.9", "db2", "ops")).as_deref(), Some("md5"));

        // db3 not listed -> no match.
        assert_eq!(p.method(&conn("10.9.9.9", "db3", "ops")), None);
    }
}
