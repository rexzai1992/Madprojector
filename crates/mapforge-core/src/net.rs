//! Sending one command to a Player over TCP, shared by Producer and by a
//! master Player that passes commands on to the other Players.

use crate::{Command, Envelope, PlayerState, PROTOCOL_VERSION};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Player wall clock minus ours, estimated at the round trip's midpoint.
pub fn estimate_clock_offset_ms(sent_ms: f64, round_trip_ms: f64, server_ms: f64) -> f64 {
    server_ms - (sent_ms + round_trip_ms / 2.0)
}

pub fn connect(address: &str, timeout: Duration) -> Result<TcpStream, String> {
    let socket = address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("invalid address")?;
    TcpStream::connect_timeout(&socket, timeout).map_err(|e| e.to_string())
}

/// Sends `command` and returns the Player's state after applying it.
pub fn exchange(address: &str, command: Command) -> Result<PlayerState, String> {
    let mut stream = connect(address, Duration::from_millis(1500))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        command_id: Uuid::new_v4(),
        command,
    };
    writeln!(stream, "{}", serde_json::to_string(&envelope).unwrap()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&response).map_err(|_| "invalid response".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_uses_round_trip_midpoint() {
        assert_eq!(estimate_clock_offset_ms(1_000.0, 40.0, 1_070.0), 50.0);
        assert_eq!(estimate_clock_offset_ms(1_000.0, 20.0, 990.0), -20.0);
    }
}
