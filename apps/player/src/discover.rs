//! Finds the other MapForge Players on this network. Every Player announces
//! itself with Bonjour (mDNS) so the iPad app can find the master; here a
//! Player listens to the same announcements. A sub can then offer the
//! masters it found instead of asking for an IP address, and can find its
//! master again after the router gave that PC another address.

use mapforge_core::PlayerRole;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

pub const SERVICE: &str = "_mapforge._tcp.local.";

/// A Player that announced itself and answered when asked for its state.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Found {
    /// The PC's name as it announces itself.
    pub name: String,
    /// Its address that answered quickest: a cable beats Wi-Fi.
    pub host: String,
    pub role: Option<PlayerRole>,
    pub this_pc: bool,
}

static FOUND: OnceLock<Mutex<Vec<Found>>> = OnceLock::new();

fn list() -> &'static Mutex<Vec<Found>> {
    FOUND.get_or_init(Default::default)
}

/// The Players found so far, by name.
pub fn found() -> Vec<Found> {
    list().lock().unwrap().clone()
}

/// The master on this network other than this PC, when there is exactly
/// one. With two masters (two shows on one network) nothing is guessed.
pub fn only_master() -> Option<Found> {
    only_master_of(&found())
}

fn only_master_of(found: &[Found]) -> Option<Found> {
    let mut masters = found
        .iter()
        .filter(|f| !f.this_pc && f.role == Some(PlayerRole::Master));
    let first = masters.next()?;
    masters.next().is_none().then(|| first.clone())
}

/// Listens for Players on `daemon` (the one this Player announces itself
/// with). `own_instance` is this Player's own announcement name.
pub fn start(daemon: &ServiceDaemon, own_instance: String) {
    let Ok(events) = daemon.browse(SERVICE) else {
        return;
    };
    thread::spawn(move || {
        // Announcement name -> (PC name, its addresses, controller port).
        let mut announced: HashMap<String, (String, Vec<Ipv4Addr>, u16)> = HashMap::new();
        let mut asked: Option<Instant> = None;
        loop {
            match events.recv_timeout(Duration::from_secs(1)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let full = info.get_fullname().to_owned();
                    let name = full
                        .strip_suffix(&format!(".{SERVICE}"))
                        .unwrap_or(&full)
                        .to_owned();
                    let mut addresses: Vec<Ipv4Addr> = info.get_addresses_v4().into_iter().collect();
                    addresses.sort();
                    announced.insert(full, (name, addresses, info.get_port()));
                    asked = None;
                }
                Ok(ServiceEvent::ServiceRemoved(_, full)) => {
                    announced.remove(&full);
                    asked = None;
                }
                Ok(_) => {}
                Err(_) if events.is_disconnected() => return,
                Err(_) => {}
            }
            // Roles change and PCs go away without saying so: ask each one
            // again every few seconds.
            if asked.is_some_and(|t| t.elapsed() < Duration::from_secs(4)) {
                continue;
            }
            asked = Some(Instant::now());
            let mut players: Vec<Found> = announced
                .values()
                .filter_map(|(name, addresses, port)| {
                    let (host, role) = ask(addresses, *port)?;
                    Some(Found {
                        name: name.strip_prefix("MapForge ").unwrap_or(name).to_owned(),
                        host,
                        role,
                        this_pc: *name == own_instance,
                    })
                })
                .collect();
            players.sort_by(|a, b| a.name.cmp(&b.name));
            *list().lock().unwrap() = players;
        }
    });
}

/// Asks a Player for its role on each of its addresses and keeps the one
/// that answered quickest. `None` when none answers.
fn ask(addresses: &[Ipv4Addr], port: u16) -> Option<(String, Option<PlayerRole>)> {
    addresses
        .iter()
        // A PC also lists its loopback address, which is no use to others.
        .filter(|ip| !ip.is_loopback())
        .filter_map(|ip| {
            let started = Instant::now();
            let body = get_state(SocketAddr::from((*ip, port)))?;
            Some((started.elapsed(), ip.to_string(), role_in(&body)))
        })
        .min_by_key(|(took, _, _)| *took)
        .map(|(_, host, role)| (host, role))
}

fn get_state(address: SocketAddr) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    stream
        .write_all(b"GET /api/state HTTP/1.1\r\nHost: mapforge\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.take(256 * 1024).read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then(|| body.to_owned())
}

fn role_in(body: &str) -> Option<PlayerRole> {
    let state: serde_json::Value = serde_json::from_str(body).ok()?;
    serde_json::from_value(state.get("role")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(name: &str, role: Option<PlayerRole>, this_pc: bool) -> Found {
        Found {
            name: name.into(),
            host: "192.168.1.10".into(),
            role,
            this_pc,
        }
    }

    #[test]
    fn the_role_is_read_from_a_players_state() {
        assert_eq!(role_in(r#"{"role":"master","transport":"playing"}"#), Some(PlayerRole::Master));
        assert_eq!(role_in(r#"{"role":"sub"}"#), Some(PlayerRole::Sub));
        assert_eq!(role_in(r#"{"role":null}"#), None);
        assert_eq!(role_in("not json"), None);
    }

    #[test]
    fn a_master_is_only_picked_when_it_is_the_only_one() {
        let master = player("stage-left", Some(PlayerRole::Master), false);
        let sub = player("stage-right", Some(PlayerRole::Sub), false);
        let me = player("this-pc", Some(PlayerRole::Master), true);
        assert_eq!(only_master_of(&[sub.clone(), master.clone(), me.clone()]), Some(master.clone()));
        // This PC being a master does not count, and neither does a sub.
        assert_eq!(only_master_of(&[sub, me]), None);
        // Two shows on one network: don't guess.
        let other = player("other-show", Some(PlayerRole::Master), false);
        assert_eq!(only_master_of(&[master, other]), None);
    }
}
