//! The host side of the agent protocol.

use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use vsock::{VsockAddr, VsockStream};

use crate::proto::{self, Request};

/// Connects to a guest's agent, retrying while the guest boots.
pub fn connect(cid: u32, wait: Duration) -> Result<VsockStream> {
    let deadline = Instant::now() + wait;
    loop {
        match VsockStream::connect(&VsockAddr::new(cid, proto::AGENT_PORT)) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("connecting to the agent of vsock cid {cid}"));
            }
        }
    }
}

/// Runs a command in the guest with `stdin` streamed in and its output
/// written to `stdout` and `stderr`; returns its exit code.
pub fn exec(
    cid: u32,
    req: &Request,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    wait: Duration,
) -> Result<i32> {
    let mut stream = connect(cid, wait)?;
    proto::write_request(&mut stream, req)?;

    let mut sender = stream.try_clone()?;
    thread::spawn(move || {
        let mut stdin = stdin;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if proto::write_frame(&mut sender, proto::STDIN, &buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = proto::write_frame(&mut sender, proto::STDIN_EOF, &[]);
    });

    loop {
        match proto::read_frame(&mut stream)? {
            Some((proto::STDOUT, data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Some((proto::STDERR, data)) => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            Some((proto::EXIT, data)) if data.len() == 4 => {
                return Ok(i32::from_be_bytes(data.try_into().unwrap()));
            }
            Some((kind, _)) => bail!("unexpected frame {kind} from the agent"),
            None => bail!("the agent closed the connection without an exit status"),
        }
    }
}

/// Runs a command and collects its output; fails on a non-zero exit.
pub fn run_captured(cid: u32, req: &Request, wait: Duration) -> Result<String> {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = exec(
        cid,
        req,
        Box::new(std::io::empty()),
        &mut out,
        &mut err,
        wait,
    )?;
    if code != 0 {
        bail!(
            "{:?} exited {code}: {}",
            req.argv,
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
