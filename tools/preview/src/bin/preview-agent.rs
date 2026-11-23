//! The guest side of `preview exec`: runs commands the host sends over vsock,
//! as the user each request names, with stdin, stdout and stderr streamed
//! both ways (see `preview::proto`).
//!
//! It takes its listening socket from systemd (`ListenStream=vsock::7000`,
//! `Accept=no`), so the port is open before the agent has started, or binds
//! it itself when started by hand. One thread per connection; a command's
//! lifetime is its connection's.
//!
//! There is no authentication: only the host can open a vsock connection to
//! a guest, and the host is root over the guest anyway.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use preview::proto::{self, Request};
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

struct User {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

fn lookup_user(name: &str) -> Result<User> {
    let passwd = std::fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 6 && f[0] == name {
            return Ok(User {
                name: name.to_string(),
                uid: f[2].parse()?,
                gid: f[3].parse()?,
                home: f[5].to_string(),
            });
        }
    }
    bail!("no user {name:?} in this guest")
}

fn listener() -> Result<VsockListener> {
    let from_systemd = std::env::var("LISTEN_PID").ok() == Some(std::process::id().to_string())
        && std::env::var("LISTEN_FDS").ok().as_deref() == Some("1");
    if from_systemd {
        // SAFETY: systemd passed exactly one listening socket, as fd 3.
        return Ok(unsafe { VsockListener::from_raw_fd(3) });
    }
    let port = std::env::args()
        .nth(1)
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(proto::AGENT_PORT);
    VsockListener::bind(&VsockAddr::new(VMADDR_CID_ANY, port))
        .with_context(|| format!("binding vsock port {port}"))
}

fn main() -> Result<()> {
    let listener = listener()?;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(e) = serve(stream) {
                        eprintln!("preview-agent: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("preview-agent: accept: {e}"),
        }
    }
    Ok(())
}

fn serve(stream: VsockStream) -> Result<()> {
    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let req = proto::read_request(&mut reader).context("reading the request")?;
    let code = match run(req, reader, &writer) {
        Ok(code) => code,
        Err(e) => {
            let msg = format!("preview-agent: {e:#}\n");
            let mut w = writer.lock().unwrap();
            let _ = proto::write_frame(&mut *w, proto::STDERR, msg.as_bytes());
            127
        }
    };
    let mut w = writer.lock().unwrap();
    proto::write_frame(&mut *w, proto::EXIT, &code.to_be_bytes())?;
    let _ = w.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn run(req: Request, mut reader: VsockStream, writer: &Arc<Mutex<VsockStream>>) -> Result<i32> {
    let user = lookup_user(&req.user)?;
    let (program, args) = req
        .argv
        .split_first()
        .ok_or_else(|| anyhow!("empty argv"))?;

    let path = std::env::var("PREVIEW_AGENT_PATH").unwrap_or_else(|_| "/bin".into());
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PATH".into(), path);
    env.insert("HOME".into(), user.home.clone());
    env.insert("USER".into(), user.name.clone());
    env.insert("LOGNAME".into(), user.name.clone());
    env.insert("SHELL".into(), "/bin/sh".into());
    for key in ["SSL_CERT_FILE", "LANG", "TZDIR"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    env.extend(req.env);

    // Looked up on the request's PATH, not the agent's own.
    let resolved = if program.contains('/') {
        program.clone()
    } else {
        which(program, &env["PATH"]).unwrap_or_else(|| program.clone())
    };
    let mut cmd = Command::new(&resolved);
    cmd.arg0(program)
        .args(args)
        .env_clear()
        .envs(&env)
        .current_dir(req.cwd.as_deref().unwrap_or(&user.home))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if user.uid != 0 {
        // std drops the supplementary groups when it changes the uid as root.
        cmd.uid(user.uid).gid(user.gid);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {program} as {}", user.name))?;

    let mut stdin = child.stdin.take();
    let pump = thread::spawn(move || {
        // Client → child stdin, until STDIN_EOF or the connection ends.
        while let Ok(Some((kind, data))) = proto::read_frame(&mut reader) {
            match kind {
                proto::STDIN => {
                    if let Some(s) = stdin.as_mut()
                        && s.write_all(&data).is_err()
                    {
                        stdin = None;
                    }
                }
                proto::STDIN_EOF => {
                    stdin = None;
                }
                _ => break,
            }
        }
    });

    let forward = |src: Box<dyn Read + Send>, kind: u8| {
        let writer = Arc::clone(writer);
        thread::spawn(move || {
            let mut src = src;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut w = writer.lock().unwrap();
                        if proto::write_frame(&mut *w, kind, &buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };
    let out = forward(Box::new(child.stdout.take().unwrap()), proto::STDOUT);
    let err = forward(Box::new(child.stderr.take().unwrap()), proto::STDERR);

    let status = child.wait()?;
    let _ = out.join();
    let _ = err.join();
    // The pump ends when the client sends EOF or hangs up; it holds nothing
    // the exit frame needs, so it is left to finish on its own.
    drop(pump);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

fn which(program: &str, path: &str) -> Option<String> {
    path.split(':')
        .map(|dir| format!("{dir}/{program}"))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
}
