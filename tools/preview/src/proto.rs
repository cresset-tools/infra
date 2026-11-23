//! The wire protocol between `preview` on the host and `preview-agent` in a
//! guest, over one vsock stream per command.
//!
//! The client opens with a [`Request`] as one line of JSON. After that both
//! directions carry frames: a kind byte, a big-endian `u32` length, and that
//! many bytes. The client sends [`STDIN`] frames and one [`STDIN_EOF`]; the
//! agent sends [`STDOUT`] and [`STDERR`] frames and ends with one [`EXIT`],
//! whose payload is the exit code as a big-endian `i32`.
//!
//! The streams are bytes, not lines, so a binary piped through stdin arrives
//! intact: `magebuild deploy` uploads its own agent that way.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const AGENT_PORT: u32 = 7000;

pub const STDIN: u8 = 0;
pub const STDIN_EOF: u8 = 1;
pub const STDOUT: u8 = 2;
pub const STDERR: u8 = 3;
pub const EXIT: u8 = 4;

/// The largest frame either side sends or accepts.
pub const MAX_FRAME: usize = 1 << 20;
/// The largest request line the agent reads.
pub const MAX_REQUEST: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// The user to run as: a name from the guest's /etc/passwd.
    pub user: String,
    /// The program and its arguments. The program is looked up on the agent's
    /// PATH unless it contains a slash.
    pub argv: Vec<String>,
    /// The working directory; the user's home when absent.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Added to the environment the agent sets up (PATH, HOME, USER, ...).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    for chunk in data
        .chunks(MAX_FRAME)
        .chain(data.is_empty().then_some(&[][..]))
    {
        let mut head = [0u8; 5];
        head[0] = kind;
        head[1..].copy_from_slice(&(chunk.len() as u32).to_be_bytes());
        w.write_all(&head)?;
        w.write_all(chunk)?;
    }
    w.flush()
}

/// Reads one frame; `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..])?;
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((head[0], data)))
}

pub fn write_request(w: &mut impl Write, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Reads the request line byte by byte, so nothing after it is consumed: the
/// frames that follow belong to the stdin pump.
pub fn read_request(r: &mut impl Read) -> io::Result<Request> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        r.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request line too long",
            ));
        }
    }
    serde_json::from_slice(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_split_at_the_limit() {
        let big = vec![7u8; MAX_FRAME + 3];
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"hello").unwrap();
        write_frame(&mut buf, STDIN_EOF, b"").unwrap();
        write_frame(&mut buf, STDERR, &big).unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Some((STDOUT, b"hello".to_vec()))
        );
        assert_eq!(read_frame(&mut r).unwrap(), Some((STDIN_EOF, vec![])));
        let (k, a) = read_frame(&mut r).unwrap().unwrap();
        let (_, b) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!((k, a.len() + b.len()), (STDERR, MAX_FRAME + 3));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn the_request_line_leaves_the_frames_alone() {
        let req = Request {
            user: "magento".into(),
            argv: vec!["true".into()],
            cwd: None,
            env: BTreeMap::new(),
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        write_frame(&mut buf, STDIN, b"x").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_request(&mut r).unwrap().user, "magento");
        assert_eq!(read_frame(&mut r).unwrap(), Some((STDIN, b"x".to_vec())));
    }
}
