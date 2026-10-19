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
    ImportHerdr { path: PathBuf },
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
            ClientMsg::ImportHerdr { path } => write_frame(w, 4, path.as_os_str().as_encoded_bytes()),
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
            4 => Ok(ClientMsg::ImportHerdr { path: PathBuf::from(String::from_utf8_lossy(&p).into_owned()) }),
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
