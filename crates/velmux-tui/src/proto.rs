//! Client ⇄ server framing: `tag: u8, len: u32 BE, payload`.
//!
//! The server owns all UI state and renders; the client only forwards raw
//! input bytes and resizes, and writes whatever ANSI it receives.

use std::io::{self, Read, Write};
use std::path::PathBuf;

pub enum ClientMsg {
    Hello { cols: u16, rows: u16, cwd: PathBuf },
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// One-shot control request (no attach): import a herdr session file.
    ImportHerdr { path: PathBuf, with_latest_claude: bool },
    /// One-shot control request: live-restart the server.
    Restart,
    /// One-shot: save everything and stop the server (panes close; restored next time).
    Stop,
    /// One-shot: the focused pane as velmux sees it (size, rows), for debugging.
    Debug,
    /// One-shot: claim a slot for the pane `velmux claim` runs in, moving it there.
    ClaimFor { pane: u64, branch: Option<String> },
    /// One-shot: release the slot that pane is in, moving it back to the base.
    ReleaseFor { pane: u64 },
    /// One-shot: `velmux claim` in a pane, saying which repo (`--repo`) and
    /// which workstream (`--workstream`, or the one the pane is in) when it's
    /// for one. Without either, the same as `ClaimFor`.
    /// `fetch` / `install`: `--no-fetch` / `--no-install` not given.
    ClaimInto { pane: u64, branch: Option<String>, repo: Option<String>, workstream: Option<String>, fetch: bool, install: bool },
    /// One-shot: an agent's hook says which session that pane's agent is in
    /// (a session reference, see `agents`).
    ReportSession { pane: u64, session: String },
    /// One-shot: every pane, where it is and what's in it (JSON).
    Panes,
    /// One-shot: close a pane (`velmux close-pane`).
    ClosePane { pane: u64 },
}

pub enum ServerMsg {
    Output(Vec<u8>),
    Bye(String),
    /// Answer to a control request.
    Reply(String),
}

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > 64 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

fn size_bytes(cols: u16, rows: u16) -> [u8; 4] {
    let (c, r) = (cols.to_be_bytes(), rows.to_be_bytes());
    [c[0], c[1], r[0], r[1]]
}

fn parse_size(p: &[u8]) -> io::Result<(u16, u16)> {
    if p.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short size"));
    }
    Ok((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])))
}

impl ClientMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMsg::Hello { cols, rows, cwd } => {
                let mut p = size_bytes(*cols, *rows).to_vec();
                p.extend_from_slice(cwd.as_os_str().as_encoded_bytes());
                write_frame(w, 1, &p)
            }
            ClientMsg::Input(bytes) => write_frame(w, 2, bytes),
            ClientMsg::Resize { cols, rows } => write_frame(w, 3, &size_bytes(*cols, *rows)),
            ClientMsg::Restart => write_frame(w, 5, &[]),
            ClientMsg::Stop => write_frame(w, 8, &[]),
            ClientMsg::Debug => write_frame(w, 9, &[]),
            ClientMsg::ClaimFor { pane, branch } => {
                let mut p = pane.to_be_bytes().to_vec();
                p.extend_from_slice(branch.as_deref().unwrap_or("").as_bytes());
                write_frame(w, 6, &p)
            }
            ClientMsg::ReleaseFor { pane } => write_frame(w, 7, &pane.to_be_bytes()),
            ClientMsg::ClaimInto { pane, branch, repo, workstream, fetch, install } => {
                let mut p = pane.to_be_bytes().to_vec();
                p.extend_from_slice(serde_json::json!({ "branch": branch, "repo": repo, "workstream": workstream, "fetch": fetch, "install": install }).to_string().as_bytes());
                write_frame(w, 11, &p)
            }
            ClientMsg::ReportSession { pane, session } => {
                let mut p = pane.to_be_bytes().to_vec();
                p.extend_from_slice(session.as_bytes());
                write_frame(w, 10, &p)
            }
            ClientMsg::Panes => write_frame(w, 12, &[]),
            ClientMsg::ClosePane { pane } => write_frame(w, 13, &pane.to_be_bytes()),
            ClientMsg::ImportHerdr { path, with_latest_claude } => {
                let mut p = vec![*with_latest_claude as u8];
                p.extend_from_slice(path.as_os_str().as_encoded_bytes());
                write_frame(w, 4, &p)
            }
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ClientMsg> {
        let (tag, p) = read_frame(r)?;
        match tag {
            1 => {
                let (cols, rows) = parse_size(&p)?;
                let cwd = PathBuf::from(String::from_utf8_lossy(&p[4..]).into_owned());
                Ok(ClientMsg::Hello { cols, rows, cwd })
            }
            2 => Ok(ClientMsg::Input(p)),
            3 => {
                let (cols, rows) = parse_size(&p)?;
                Ok(ClientMsg::Resize { cols, rows })
            }
            5 => Ok(ClientMsg::Restart),
            8 => Ok(ClientMsg::Stop),
            9 => Ok(ClientMsg::Debug),
            6 if p.len() >= 8 => {
                let branch = String::from_utf8_lossy(&p[8..]).into_owned();
                Ok(ClientMsg::ClaimFor {
                    pane: u64::from_be_bytes(p[..8].try_into().unwrap()),
                    branch: (!branch.is_empty()).then_some(branch),
                })
            }
            7 if p.len() >= 8 => Ok(ClientMsg::ReleaseFor { pane: u64::from_be_bytes(p[..8].try_into().unwrap()) }),
            12 => Ok(ClientMsg::Panes),
            13 if p.len() >= 8 => Ok(ClientMsg::ClosePane { pane: u64::from_be_bytes(p[..8].try_into().unwrap()) }),
            11 if p.len() > 8 => {
                let v: serde_json::Value = serde_json::from_slice(&p[8..]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let text = |k: &str| v[k].as_str().map(String::from);
                // (An older client sends neither: both on.)
                let on = |k: &str| v[k].as_bool().unwrap_or(true);
                Ok(ClientMsg::ClaimInto { pane: u64::from_be_bytes(p[..8].try_into().unwrap()), branch: text("branch"), repo: text("repo"), workstream: text("workstream"), fetch: on("fetch"), install: on("install") })
            }
            10 if p.len() > 8 => Ok(ClientMsg::ReportSession {
                pane: u64::from_be_bytes(p[..8].try_into().unwrap()),
                session: String::from_utf8_lossy(&p[8..]).into_owned(),
            }),
            4 if !p.is_empty() => Ok(ClientMsg::ImportHerdr {
                with_latest_claude: p[0] == 1,
                path: PathBuf::from(String::from_utf8_lossy(&p[1..]).into_owned()),
            }),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown client frame")),
        }
    }
}

impl ServerMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ServerMsg::Output(bytes) => write_frame(w, 1, bytes),
            ServerMsg::Bye(reason) => write_frame(w, 2, reason.as_bytes()),
            ServerMsg::Reply(text) => write_frame(w, 3, text.as_bytes()),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<ServerMsg> {
        let (tag, p) = read_frame(r)?;
        match tag {
            1 => Ok(ServerMsg::Output(p)),
            2 => Ok(ServerMsg::Bye(String::from_utf8_lossy(&p).into_owned())),
            3 => Ok(ServerMsg::Reply(String::from_utf8_lossy(&p).into_owned())),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown server frame")),
        }
    }
}
