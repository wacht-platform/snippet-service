//! Per-session interactive PTY. Human clients (TUI / Flutter) attach over
//! `/attach` with `wire: "term"` frames. The agent `bash` tool stays
//! non-interactive and never shares this PTY.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SCROLLBACK_CAP: usize = 256 * 1024;
const MAX_TERMS: usize = 8;
const NOTIFY_PAYLOAD_CAP: usize = 512;

/// A PTY-originated desktop notification (BEL, OSC 9, OSC 777).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermNotify {
    pub pane: String,
    pub message: String,
}

/// Detect BEL / OSC 9 / OSC 777 in the raw stream so the daemon can
/// push them over `/events` even when no client is painting the pane.
#[derive(Default)]
struct NotifyScan {
    /// 0 ground, 1 saw ESC, 2 collecting OSC payload.
    esc: u8,
    payload: Vec<u8>,
}

impl NotifyScan {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            match self.esc {
                0 if b == 0x07 => out.push(String::new()),
                0 if b == 0x1b => self.esc = 1,
                1 if b == b']' => {
                    self.esc = 2;
                    self.payload.clear();
                }
                1 => self.esc = 0,
                2 if b == 0x07 => {
                    if let Some(msg) = parse_osc_notify(&self.payload) {
                        out.push(msg);
                    }
                    self.payload.clear();
                    self.esc = 0;
                }
                2 if b == 0x1b => self.esc = 3,
                2 => {
                    if self.payload.len() < NOTIFY_PAYLOAD_CAP {
                        self.payload.push(b);
                    }
                }
                // OSC terminated by ST (`ESC \`).
                3 if b == b'\\' => {
                    if let Some(msg) = parse_osc_notify(&self.payload) {
                        out.push(msg);
                    }
                    self.payload.clear();
                    self.esc = 0;
                }
                3 => {
                    // Not ST — treat the ESC as starting a new sequence.
                    self.payload.clear();
                    self.esc = if b == b']' {
                        2
                    } else if b == 0x1b {
                        1
                    } else {
                        0
                    };
                }
                _ => self.esc = 0,
            }
        }
    }
}

fn parse_osc_notify(payload: &[u8]) -> Option<String> {
    // OSC 9 ; message
    if let Some(rest) = payload.strip_prefix(b"9;") {
        return Some(osc_message(rest));
    }
    // OSC 777 ; notify ; title ; body   (urxvt / notify-send)
    if let Some(rest) = payload.strip_prefix(b"777;") {
        let text = std::str::from_utf8(rest).unwrap_or("");
        let mut parts = text.splitn(3, ';');
        let kind = parts.next().unwrap_or("");
        if !kind.eq_ignore_ascii_case("notify") {
            return None;
        }
        let title = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        return Some(match (title.is_empty(), body.is_empty()) {
            (true, true) => String::new(),
            (false, true) => title.to_string(),
            (true, false) => body.to_string(),
            (false, false) => format!("{title}: {body}"),
        });
    }
    None
}

fn osc_message(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

/// Detect CSI 6n / CSI 5n in the raw stream and answer them. `vt100` 0.15
/// swallows DSR and never replies, which is why fish left the cursor dead.
#[derive(Default)]
struct DsrScan {
    esc: u8, // 0 ground, 1 saw ESC, 2 saw CSI
    params: Vec<u8>,
}

impl DsrScan {
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>, row: u16, col: u16) {
        for &b in bytes {
            match self.esc {
                0 if b == 0x1b => self.esc = 1,
                1 if b == b'[' => {
                    self.esc = 2;
                    self.params.clear();
                }
                1 if b == b'Z' => {
                    replies.extend_from_slice(b"\x1b[?1;2c");
                    self.esc = 0;
                }
                1 => self.esc = 0,
                2 if b.is_ascii_digit() || b == b';' || b == b'>' || b == b'?' => {
                    if self.params.len() < 16 {
                        self.params.push(b);
                    }
                }
                2 if b == b'n' => {
                    let p = std::str::from_utf8(&self.params).unwrap_or("");
                    if p.is_empty() || p == "6" {
                        replies.extend_from_slice(format!("\x1b[{};{}R", row, col).as_bytes());
                    } else if p == "5" {
                        replies.extend_from_slice(b"\x1b[0n");
                    }
                    self.esc = 0;
                    self.params.clear();
                }
                // DA / secondary DA — fish waits on these after a command.
                2 if b == b'c' => {
                    let p = std::str::from_utf8(&self.params).unwrap_or("");
                    if p == ">" || p == ">0" {
                        replies.extend_from_slice(b"\x1b[>0;276;0c");
                    } else {
                        replies.extend_from_slice(b"\x1b[?1;2c");
                    }
                    self.esc = 0;
                    self.params.clear();
                }
                2 => {
                    self.esc = 0;
                    self.params.clear();
                }
                _ => self.esc = 0,
            }
        }
    }
}

/// Per-client mailbox. Each /attach owns one; pollers never drain the PTY
/// themselves. Two attach loops calling `poll_out` stole the fish prompt
/// and left the TUI painting leftovers at column 46.
struct Fanout {
    next_client: u64,
    clients: HashMap<u64, Vec<(String, Vec<u8>, u16, u16, bool)>>,
}

impl Fanout {
    fn new() -> Self {
        Self {
            next_client: 1,
            clients: HashMap::new(),
        }
    }
}

/// Several human PTYs for one session. Frames carry `id` (default `"0"`).
pub struct SessionTerms {
    cwd: PathBuf,
    next_id: Mutex<u32>,
    terms: Mutex<HashMap<String, Arc<SessionTerm>>>,
    /// Pane ids that must send a full snapshot on the next attach poll
    /// (open / resize). Incremental `out` frames are diffs; a client that
    /// missed them (or rebuilt its screen) needs the whole scrollback.
    snap_ids: Mutex<std::collections::HashSet<String>>,
    fanout: Mutex<Fanout>,
    /// BEL / OSC 9 / OSC 777 seen since the last `/events` drain.
    notifies: Mutex<Vec<TermNotify>>,
    /// Incremental frames drained while no /attach client was subscribed.
    /// The next `subscribe` takes these so a harvest for notifications
    /// cannot steal bytes from a later attach.
    pending: Mutex<Vec<(String, Vec<u8>, u16, u16, bool)>>,
}

/// Token for one /attach subscriber. Drop unregisters the mailbox.
pub struct TermClient {
    id: u64,
    terms: Arc<SessionTerms>,
}

impl Drop for TermClient {
    fn drop(&mut self) {
        if let Ok(mut g) = self.terms.fanout.lock() {
            g.clients.remove(&self.id);
        }
    }
}

impl SessionTerms {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            next_id: Mutex::new(1),
            terms: Mutex::new(HashMap::new()),
            snap_ids: Mutex::new(std::collections::HashSet::new()),
            fanout: Mutex::new(Fanout::new()),
            notifies: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Drain the PTY (so idle shells still fire BEL / OSC) and take
    /// any notifications queued since the last harvest.
    pub fn harvest(&self) -> Vec<TermNotify> {
        self.pump();
        self.take_notifies()
    }

    /// Drain PTY-originated notifications (BEL / OSC 9 / OSC 777).
    pub fn take_notifies(&self) -> Vec<TermNotify> {
        match self.notifies.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => Vec::new(),
        }
    }

    fn push_notifies(&self, pane: &str, messages: Vec<String>) {
        if messages.is_empty() {
            return;
        }
        if let Ok(mut g) = self.notifies.lock() {
            for message in messages {
                g.push(TermNotify {
                    pane: pane.to_string(),
                    message,
                });
            }
            // Bound the mailbox so a looping `echo -e '\a'` cannot grow forever
            // if no /events client is attached.
            if g.len() > 32 {
                let drop = g.len() - 32;
                g.drain(..drop);
            }
        }
    }

    pub fn subscribe(self: &Arc<Self>) -> TermClient {
        let mut g = self.fanout.lock().expect("fanout");
        let id = g.next_client;
        g.next_client = g.next_client.wrapping_add(1).max(1);
        let mailbox = match self.pending.lock() {
            Ok(mut p) => std::mem::take(&mut *p),
            Err(_) => Vec::new(),
        };
        g.clients.insert(id, mailbox);
        TermClient {
            id,
            terms: Arc::clone(self),
        }
    }

    pub fn get_or_create(&self, id: &str) -> Option<Arc<SessionTerm>> {
        let mut g = self.terms.lock().ok()?;
        if let Some(t) = g.get(id) {
            return Some(t.clone());
        }
        if g.len() >= MAX_TERMS {
            return None;
        }
        let t = SessionTerm::new(self.cwd.clone());
        g.insert(id.to_string(), t.clone());
        Some(t)
    }

    pub fn get(&self, id: &str) -> Option<Arc<SessionTerm>> {
        self.terms.lock().ok()?.get(id).cloned()
    }

    pub fn request_snapshot(&self, id: &str) {
        if let Ok(mut g) = self.snap_ids.lock() {
            g.insert(id.to_string());
        }
    }

    pub fn take_snapshots(&self) -> Vec<(String, Vec<u8>, u16, u16, bool)> {
        let ids: Vec<String> = match self.snap_ids.lock() {
            Ok(mut g) => g.drain().collect(),
            Err(_) => return Vec::new(),
        };
        ids.into_iter()
            .filter_map(|id| {
                let t = self.get(&id)?;
                let (bytes, cols, rows, alive) = t.snapshot();
                Some((id, bytes, cols, rows, alive))
            })
            .collect()
    }

    pub fn alloc_id(&self) -> String {
        let mut n = match self.next_id.lock() {
            Ok(g) => g,
            Err(_) => return "0".into(),
        };
        let id = n.to_string();
        *n += 1;
        id
    }

    pub fn close(&self, id: &str) {
        if let Ok(mut g) = self.terms.lock() {
            if let Some(t) = g.remove(id) {
                t.kill();
            }
        }
    }

    /// Drain the PTY once and copy the same bytes to every subscriber.
    fn pump(&self) {
        let panes: Vec<(String, Arc<SessionTerm>)> = {
            let g = match self.terms.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            g.iter().map(|(id, t)| (id.clone(), t.clone())).collect()
        };
        let mut frames = Vec::new();
        for (id, t) in panes {
            let was_alive = t.is_alive();
            let (chunk, notes) = t.poll_out();
            self.push_notifies(&id, notes);
            let (_, cols, rows, alive) = t.snapshot();
            if chunk.is_empty() && alive {
                continue;
            }
            if chunk.is_empty() && !was_alive {
                // Already reported dead.
                continue;
            }
            frames.push((id, chunk, cols, rows, alive));
        }
        if frames.is_empty() {
            return;
        }
        if let Ok(mut g) = self.fanout.lock() {
            if g.clients.is_empty() {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.extend(frames);
                    if pending.len() > 64 {
                        let drop = pending.len() - 64;
                        pending.drain(..drop);
                    }
                }
            } else {
                for mailbox in g.clients.values_mut() {
                    mailbox.extend(frames.iter().cloned());
                }
            }
        }
    }

    pub fn poll_client(&self, client: &TermClient) -> Vec<(String, Vec<u8>, u16, u16, bool)> {
        self.pump();
        match self.fanout.lock() {
            Ok(mut g) => g
                .clients
                .get_mut(&client.id)
                .map(std::mem::take)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Tests / single-consumer path. Prefer `subscribe` + `poll_client`.
    pub fn poll_all(&self) -> Vec<(String, Vec<u8>, u16, u16, bool)> {
        let panes: Vec<(String, Arc<SessionTerm>)> = {
            let g = match self.terms.lock() {
                Ok(g) => g,
                Err(_) => return Vec::new(),
            };
            g.iter().map(|(id, t)| (id.clone(), t.clone())).collect()
        };
        panes
            .into_iter()
            .map(|(id, t)| {
                let (chunk, notes) = t.poll_out();
                self.push_notifies(&id, notes);
                let (_, cols, rows, alive) = t.snapshot();
                (id, chunk, cols, rows, alive)
            })
            .collect()
    }

    pub fn snapshot_all(&self) -> Vec<(String, Vec<u8>, u16, u16, bool)> {
        let g = match self.terms.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        g.iter()
            .map(|(id, t)| {
                let (bytes, cols, rows, alive) = t.snapshot();
                (id.clone(), bytes, cols, rows, alive)
            })
            .collect()
    }
}

pub struct SessionTerm {
    inner: Mutex<Inner>,
}

struct Inner {
    child_pid: Option<libc::pid_t>,
    master: Option<std::fs::File>,
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    scrollback: VecDeque<u8>,
    alive: bool,
    dsr: DsrScan,
    notify: NotifyScan,
}

impl SessionTerm {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                child_pid: None,
                master: None,
                cols: 80,
                rows: 24,
                cwd,
                scrollback: VecDeque::with_capacity(4096),
                alive: false,
                dsr: DsrScan::default(),
                notify: NotifyScan::default(),
            }),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.inner.lock().ok().map(|i| i.alive).unwrap_or(false)
    }

    pub fn ensure(&self, cols: u16, rows: u16) -> Result<(), String> {
        let mut g = self.inner.lock().map_err(|e| e.to_string())?;
        if let Some(pid) = g.child_pid {
            if !reap_if_exited(pid) {
                if cols != g.cols || rows != g.rows {
                    g.cols = cols.max(2);
                    g.rows = rows.max(2);
                    if let Some(m) = g.master.as_ref() {
                        set_winsize(m.as_raw_fd(), g.cols, g.rows);
                    }
                }
                return Ok(());
            }
            g.child_pid = None;
            g.master = None;
            g.alive = false;
        }
        let r = spawn_locked(&mut g, cols, rows);
        r
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(m) = g.master.as_mut() {
            let _ = m.write_all(bytes);
            let _ = m.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.cols = cols.max(2);
        g.rows = rows.max(2);
        if let Some(m) = g.master.as_ref() {
            set_winsize(m.as_raw_fd(), g.cols, g.rows);
        }
    }

    /// Drain newly available PTY output into scrollback and return it
    /// plus any BEL / OSC 9 / OSC 777 messages found in this chunk.
    pub fn poll_out(&self) -> (Vec<u8>, Vec<String>) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return (Vec::new(), Vec::new()),
        };
        let Some(master) = g.master.as_mut() else {
            return (Vec::new(), Vec::new());
        };
        let mut buf = [0u8; 8192];
        let mut out = Vec::new();
        // Bound one poll so a noisy fish (or a broken TTY loop) cannot
        // pin this lock and starve the rest of the daemon.
        for _ in 0..32 {
            match master.read(&mut buf) {
                Ok(0) => {
                    g.alive = false;
                    break;
                }
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    g.alive = false;
                    break;
                }
            }
        }
        if let Some(pid) = g.child_pid {
            if reap_if_exited(pid) {
                g.alive = false;
                g.child_pid = None;
            }
        }
        if !out.is_empty() {
            for b in &out {
                if g.scrollback.len() >= SCROLLBACK_CAP {
                    g.scrollback.pop_front();
                }
                g.scrollback.push_back(*b);
            }
            // Answer DSR/DA here so fish gets a reply even if a client
            // never paints (or two clients would double-answer).
            let row = g.rows.max(1);
            let col = 1u16;
            let mut replies = Vec::new();
            g.dsr.feed(&out, &mut replies, row, col);
            if !replies.is_empty() {
                if let Some(m) = g.master.as_mut() {
                    let _ = m.write_all(&replies);
                    let _ = m.flush();
                }
            }
        }
        let mut notes = Vec::new();
        g.notify.feed(&out, &mut notes);
        (out, notes)
    }

    pub fn snapshot(&self) -> (Vec<u8>, u16, u16, bool) {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return (Vec::new(), 80, 24, false),
        };
        (
            g.scrollback.iter().copied().collect(),
            g.cols,
            g.rows,
            g.alive,
        )
    }

    pub fn kill(&self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(pid) = g.child_pid.take() {
                unsafe {
                    libc::kill(pid, libc::SIGHUP);
                    libc::kill(pid, libc::SIGTERM);
                    let mut status = 0;
                    libc::waitpid(pid, &mut status, 0);
                }
            }
            g.master = None;
            g.alive = false;
        }
    }
}

fn reap_if_exited(pid: libc::pid_t) -> bool {
    unsafe {
        let mut status = 0;
        let r = libc::waitpid(pid, &mut status, libc::WNOHANG);
        r == pid || r < 0
    }
}

fn spawn_locked(g: &mut Inner, cols: u16, rows: u16) -> Result<(), String> {
    g.cols = cols.max(2);
    g.rows = rows.max(2);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let cwd = if g.cwd.is_dir() {
        g.cwd.clone()
    } else {
        PathBuf::from(".")
    };
    let ws = libc::winsize {
        ws_row: g.rows,
        ws_col: g.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master_fd = -1;
    let pid = unsafe { libc::forkpty(&mut master_fd, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if pid < 0 {
        return Err(format!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child: forkpty already made the slave our controlling TTY on 0/1/2.
        let _ = std::env::set_current_dir(&cwd);
        unsafe {
            libc::setenv(c"TERM".as_ptr(), c"xterm-256color".as_ptr(), 1);
            libc::setenv(c"COLORTERM".as_ptr(), c"truecolor".as_ptr(), 1);
            let cshell =
                std::ffi::CString::new(shell.as_bytes()).unwrap_or_else(|_| c"/bin/bash".into());
            libc::setenv(c"SHELL".as_ptr(), cshell.as_ptr(), 1);
            let argv = [cshell.as_ptr(), c"-i".as_ptr(), std::ptr::null()];
            libc::execvp(cshell.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    // Parent
    set_nonblocking(master_fd);
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    g.child_pid = Some(pid);
    g.master = Some(master);
    g.alive = true;
    Ok(())
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn set_winsize(fd: i32, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Screen buffer for the TUI pane. Parsing is `vt100` (vte-based), not our
/// homemade CSI machine — fish/zsh prompts need a real emulator.
pub struct VtScreen {
    pub cols: usize,
    pub rows: usize,
    parser: vt100::Parser,
    replies: Vec<u8>,
    dsr: DsrScan,
    /// Full PTY stream so a resize can rebuild the screen instead of
    /// leaving an empty grid that later `out` diffs never refill.
    history: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VtCell {
    pub ch: char,
    pub fg: u8,
    pub bg: u8,
    pub bold: bool,
    pub inverse: bool,
}

impl Default for VtCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: 7,
            bg: 0,
            bold: false,
            inverse: false,
        }
    }
}

fn map_color(c: vt100::Color, default: u8) -> u8 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => i,
        vt100::Color::Rgb(r, g, b) => rgb_to_256(r, g, b),
    }
}

impl VtScreen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(2);
        Self {
            cols,
            rows,
            parser: vt100::Parser::new(rows as u16, cols as u16, 0),
            replies: Vec::new(),
            dsr: DsrScan::default(),
            history: Vec::new(),
        }
    }

    /// Bytes the emulator must write back to the PTY (DSR / cursor report).
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        // Keep the live grid. Replaying history here stacked a second
        // prompt/listing on top of the PTY's own redraw.
        self.parser.set_size(rows as u16, cols as u16);
    }

    pub fn cell(&self, x: usize, y: usize) -> VtCell {
        let Some(c) = self.parser.screen().cell(y as u16, x as u16) else {
            return VtCell::default();
        };
        let text = c.contents();
        let ch = text.chars().next().unwrap_or(' ');
        let inverse = c.inverse();
        VtCell {
            ch: if text.is_empty() { ' ' } else { ch },
            fg: map_color(c.fgcolor(), 7),
            bg: map_color(c.bgcolor(), 0),
            bold: c.bold(),
            inverse,
        }
    }

    pub fn cursor(&self) -> (usize, usize) {
        let (row, col) = self.parser.screen().cursor_position();
        (col as usize, row as usize)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Answer DSR against the *current* cursor, then apply the bytes so
        // the report matches what the shell just asked about.
        let (row, col) = self.parser.screen().cursor_position();
        self.dsr.feed(
            bytes,
            &mut self.replies,
            row.saturating_add(1),
            col.saturating_add(1),
        );
        self.parser.process(bytes);
        self.history.extend_from_slice(bytes);
        const HISTORY_CAP: usize = 256 * 1024;
        if self.history.len() > HISTORY_CAP {
            let drop = self.history.len() - HISTORY_CAP;
            self.history.drain(..drop);
        }
    }
}

fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (232 + ((r as u16 - 8) * 24 / 247)) as u8;
    }
    let ri = (r as u16 * 5 / 255) as u8;
    let gi = (g as u16 * 5 / 255) as u8;
    let bi = (b as u16 * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_sgr() {
        let mut s = VtScreen::new(20, 4);
        s.feed(b"\x1b[31mhi\x1b[0m");
        assert_eq!(s.cell(0, 0).ch, 'h');
        assert_eq!(s.cell(1, 0).ch, 'i');
        assert_eq!(s.cell(0, 0).fg, 1);
        assert_eq!(s.cell(2, 0).fg, 7);
    }

    #[test]
    fn cup_and_ed() {
        let mut s = VtScreen::new(10, 3);
        s.feed(b"abc\x1b[1;1H\x1b[2Jxyz");
        assert_eq!(s.cell(0, 0).ch, 'x');
        assert_eq!(s.cell(1, 0).ch, 'y');
    }

    #[test]
    fn wrap_does_not_overwrite_last_col() {
        let mut s = VtScreen::new(4, 2);
        s.feed(b"abcdX");
        assert_eq!(s.cell(0, 0).ch, 'a');
        assert_eq!(s.cell(3, 0).ch, 'd');
        assert_eq!(s.cell(0, 1).ch, 'X');
        assert_eq!(s.cursor(), (1, 1));
    }

    #[test]
    fn dsr_reports_cursor() {
        let mut s = VtScreen::new(20, 4);
        s.feed(b"hi");
        s.feed(b"\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[1;3R");
    }

    #[test]
    fn da_and_secondary_da_reply() {
        let mut s = VtScreen::new(20, 4);
        s.feed(b"\x1b[c");
        assert_eq!(s.take_replies(), b"\x1b[?1;2c");
        s.feed(b"\x1b[>c");
        assert_eq!(s.take_replies(), b"\x1b[>0;276;0c");
    }

    #[test]
    fn notify_scan_bell_and_osc() {
        let mut s = NotifyScan::default();
        let mut out = Vec::new();
        s.feed(b"ok\x07", &mut out);
        s.feed(b"\x1b]9;build done\x07", &mut out);
        s.feed(b"\x1b]777;notify;cargo;finished\x1b\\", &mut out);
        s.feed(b"\x1b]0;window title\x07", &mut out);
        assert_eq!(out, vec!["", "build done", "cargo: finished"]);
    }

    #[test]
    fn resize_keeps_visible_cells() {
        let mut s = VtScreen::new(20, 4);
        s.feed(b"hello");
        s.resize(40, 8);
        assert_eq!(s.cell(0, 0).ch, 'h');
        assert_eq!(s.cell(4, 0).ch, 'o');
    }

    #[test]
    fn forkpty_shell_echoes() {
        let term = SessionTerm::new(std::env::temp_dir());
        term.ensure(40, 12).expect("spawn");
        // Force bash so this is not fish-config dependent.
        // The spawned SHELL may still be fish; send a command that any POSIX
        // shell prints.
        std::thread::sleep(std::time::Duration::from_millis(80));
        term.write(b"printf SNIPPET_PTY_OK\\n\n");
        let mut got = String::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let (chunk, _) = term.poll_out();
            got.push_str(&String::from_utf8_lossy(&chunk));
            if got.contains("SNIPPET_PTY_OK") {
                term.kill();
                return;
            }
        }
        term.kill();
        panic!("no echo from PTY, got: {got:?}");
    }

    fn screen_text(s: &VtScreen) -> String {
        let mut out = String::new();
        for y in 0..s.rows {
            let mut line = String::new();
            for x in 0..s.cols {
                line.push(s.cell(x, y).ch);
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    fn pump(term: &SessionTerm, vt: &mut VtScreen, ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        loop {
            let (chunk, _) = term.poll_out();
            if !chunk.is_empty() {
                vt.feed(&chunk);
                let replies = vt.take_replies();
                if !replies.is_empty() {
                    term.write(&replies);
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn shell_ls_paints(shell: &str) {
        if !std::path::Path::new(shell).exists() {
            return;
        }
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("AAA.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("BBB.txt"), b"y").unwrap();
        // SAFETY: test-only, sequential --test-threads=1.
        unsafe {
            std::env::set_var("SHELL", shell);
        }
        let term = SessionTerm::new(dir.path().to_path_buf());
        term.ensure(80, 24).expect("spawn");
        let mut vt = VtScreen::new(80, 24);
        pump(&term, &mut vt, 800);
        term.write(b"ls\r");
        pump(&term, &mut vt, 800);
        term.write(b"ls\r");
        pump(&term, &mut vt, 800);
        unsafe {
            std::env::remove_var("SHELL");
        }
        let painted = screen_text(&vt);
        term.kill();
        assert!(
            painted.matches("ls").count() >= 2,
            "{shell}: expected two visible ls commands, got:\n{painted}"
        );
        assert!(
            painted.contains("AAA.txt") && painted.contains("BBB.txt"),
            "{shell}: expected listing, got:\n{painted}"
        );
        let glued = painted
            .lines()
            .any(|l| l.contains("ls") && l.contains("AAA.txt") && !l.contains("> ls"));
        assert!(
            !glued,
            "{shell}: ls listing glued onto the command line:\n{painted}"
        );
    }

    #[test]
    fn fish_ls_paints_command_and_listing() {
        shell_ls_paints("/usr/bin/fish");
    }

    #[test]
    fn bash_ls_paints_command_and_listing() {
        shell_ls_paints("/bin/bash");
    }

    #[test]
    fn zsh_ls_paints_command_and_listing() {
        let zsh = ["/usr/bin/zsh", "/bin/zsh"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists());
        if let Some(zsh) = zsh {
            shell_ls_paints(zsh);
        }
    }

    #[test]
    fn two_panes_spawn_and_echo_independently() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("AAA.txt"), b"x").unwrap();
        let terms = SessionTerms::new(dir.path().to_path_buf());
        let a = terms.get_or_create("0").expect("pane 0");
        let b = terms.get_or_create("4").expect("pane 4");
        a.ensure(80, 24).expect("spawn 0");
        b.ensure(80, 24).expect("spawn 4");
        let mut vt0 = VtScreen::new(80, 24);
        let mut vt4 = VtScreen::new(80, 24);
        fn pump_one(t: &SessionTerm, vt: &mut VtScreen, ms: u64) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            loop {
                let (chunk, _) = t.poll_out();
                if !chunk.is_empty() {
                    vt.feed(&chunk);
                    let replies = vt.take_replies();
                    if !replies.is_empty() {
                        t.write(&replies);
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        pump_one(&a, &mut vt0, 600);
        pump_one(&b, &mut vt4, 600);
        a.write(b"echo PANE0\r");
        b.write(b"echo PANE4\r");
        pump_one(&a, &mut vt0, 800);
        pump_one(&b, &mut vt4, 800);
        let t0 = screen_text(&vt0);
        let t4 = screen_text(&vt4);
        a.kill();
        b.kill();
        assert!(
            t0.contains("PANE0"),
            "pane 0 should echo its own command:\n{t0}"
        );
        assert!(
            t4.contains("PANE4"),
            "pane 4 should echo its own command:\n{t4}"
        );
        assert!(
            !t0.contains("PANE4"),
            "pane 0 must not show pane 4 output:\n{t0}"
        );
        assert!(
            !t4.contains("PANE0"),
            "pane 4 must not show pane 0 output:\n{t4}"
        );
    }

    #[test]
    fn two_clients_both_see_the_same_bytes() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let terms = SessionTerms::new(dir.path().to_path_buf());
        let a = terms.subscribe();
        let b = terms.subscribe();
        let pane = terms.get_or_create("0").expect("pane");
        pane.ensure(80, 24).expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        let mut fa = Vec::new();
        let mut fb = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, chunk, _, _, _) in terms.poll_client(&a) {
                fa.extend(chunk);
            }
            for (_, chunk, _, _, _) in terms.poll_client(&b) {
                fb.extend(chunk);
            }
            if !fa.is_empty() && !fb.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        pane.kill();
        let sa = String::from_utf8_lossy(&fa);
        let sb = String::from_utf8_lossy(&fb);
        assert!(
            !fa.is_empty() && fa == fb,
            "both attach clients must get the same PTY bytes\na={sa:?}\nb={sb:?}"
        );
    }
}
