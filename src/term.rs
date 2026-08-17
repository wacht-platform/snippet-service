//! Per-session interactive PTY. Human clients (TUI / Flutter) attach over
//! `/attach` with `wire: "term"` frames. The agent `bash` tool stays
//! non-interactive and never shares this PTY.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SCROLLBACK_CAP: usize = 256 * 1024;
const MAX_TERMS: usize = 8;

/// Several human PTYs for one session. Frames carry `id` (default `"0"`).
pub struct SessionTerms {
    cwd: PathBuf,
    next_id: Mutex<u32>,
    terms: Mutex<std::collections::HashMap<String, Arc<SessionTerm>>>,
}

impl SessionTerms {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            next_id: Mutex::new(1),
            terms: Mutex::new(std::collections::HashMap::new()),
        })
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

    pub fn poll_all(&self) -> Vec<(String, Vec<u8>, u16, u16, bool)> {
        let g = match self.terms.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        g.iter()
            .map(|(id, t)| {
                let chunk = t.poll_out();
                let (_, cols, rows, alive) = t.snapshot();
                (id.clone(), chunk, cols, rows, alive)
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
    child: Option<Child>,
    master: Option<std::fs::File>,
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    scrollback: VecDeque<u8>,
    alive: bool,
}

impl SessionTerm {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                child: None,
                master: None,
                cols: 80,
                rows: 24,
                cwd,
                scrollback: VecDeque::with_capacity(4096),
                alive: false,
            }),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.inner.lock().ok().map(|i| i.alive).unwrap_or(false)
    }

    pub fn ensure(&self, cols: u16, rows: u16) -> Result<(), String> {
        let mut g = self.inner.lock().map_err(|e| e.to_string())?;
        if let Some(child) = g.child.as_mut() {
            if let Ok(None) = child.try_wait() {
                if cols != g.cols || rows != g.rows {
                    g.cols = cols.max(2);
                    g.rows = rows.max(2);
                    if let Some(m) = g.master.as_ref() {
                        set_winsize(m.as_raw_fd(), g.cols, g.rows);
                    }
                }
                return Ok(());
            }
        }
        spawn_locked(&mut g, cols, rows)
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

    /// Drain newly available PTY output into scrollback and return it.
    pub fn poll_out(&self) -> Vec<u8> {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let Some(master) = g.master.as_mut() else {
            return Vec::new();
        };
        let mut buf = [0u8; 8192];
        let mut out = Vec::new();
        loop {
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
        if let Some(child) = g.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                g.alive = false;
            }
        }
        if !out.is_empty() {
            for b in &out {
                if g.scrollback.len() >= SCROLLBACK_CAP {
                    g.scrollback.pop_front();
                }
                g.scrollback.push_back(*b);
            }
        }
        out
    }

    pub fn snapshot(&self) -> (Vec<u8>, u16, u16, bool) {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return (Vec::new(), 80, 24, false),
        };
        (g.scrollback.iter().copied().collect(), g.cols, g.rows, g.alive)
    }

    pub fn kill(&self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(mut c) = g.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            g.master = None;
            g.alive = false;
        }
    }
}

fn spawn_locked(g: &mut Inner, cols: u16, rows: u16) -> Result<(), String> {
    g.cols = cols.max(2);
    g.rows = rows.max(2);
    let (master, slave) = open_pty()?;
    set_winsize(master.as_raw_fd(), g.cols, g.rows);
    set_nonblocking(master.as_raw_fd());

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let cwd = if g.cwd.is_dir() {
        g.cwd.clone()
    } else {
        PathBuf::from(".")
    };
    let mut cmd = Command::new(&shell);
    // Interactive login-style session. Fish/zsh job control needs a real
    // controlling TTY on fds 0/1/2 — dup'd slave clones without login_tty
    // leave tcgetpgrp/setpgid failing and some shells spin until the host OOMs.
    cmd.arg("-i")
        .current_dir(&cwd)
        .stdin(Stdio::from(dup_file(&slave)?))
        .stdout(Stdio::from(dup_file(&slave)?))
        .stderr(Stdio::from(dup_file(&slave)?))
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("SHELL", &shell);
    let slave_fd = slave.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Make this the session's controlling terminal, then put stdin/
            // stdout/stderr on the slave and close extras so job control works.
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EPERM) {
                    return Err(err);
                }
            }
            if libc::dup2(slave_fd, 0) < 0
                || libc::dup2(slave_fd, 1) < 0
                || libc::dup2(slave_fd, 2) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            let pgid = libc::getpid();
            let _ = libc::setpgid(0, pgid);
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("spawn shell: {e}"))?;
    drop(slave);
    g.child = Some(child);
    g.master = Some(master);
    g.alive = true;
    // Give the shell a tick to emit its first prompt.
    thread::sleep(Duration::from_millis(20));
    Ok(())
}

fn open_pty() -> Result<(std::fs::File, std::fs::File), String> {
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master_fd < 0 {
            return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
        }
        if libc::grantpt(master_fd) != 0 {
            libc::close(master_fd);
            return Err(format!("grantpt: {}", std::io::Error::last_os_error()));
        }
        if libc::unlockpt(master_fd) != 0 {
            libc::close(master_fd);
            return Err(format!("unlockpt: {}", std::io::Error::last_os_error()));
        }
        let mut name = [0i8; 128];
        if libc::ptsname_r(master_fd, name.as_mut_ptr(), name.len()) != 0 {
            libc::close(master_fd);
            return Err(format!("ptsname_r: {}", std::io::Error::last_os_error()));
        }
        let cname = std::ffi::CStr::from_ptr(name.as_ptr());
        let slave_path = Path::new(cname.to_str().map_err(|e| e.to_string())?);
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(slave_path)
            .map_err(|e| format!("open slave: {e}"))?;
        let master = std::os::fd::FromRawFd::from_raw_fd(master_fd);
        Ok((master, slave))
    }
}

fn dup_file(f: &std::fs::File) -> Result<std::fs::File, String> {
    f.try_clone().map_err(|e| e.to_string())
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

/// Tiny VT parser used by the TUI to paint the session PTY into a ratatui pane.
/// Enough for bash/vim/htop/less: SGR, CUP, ED/EL, cursor save/restore, alt
/// screen, CUU/CUD/CUF/CUB, CHA, VPA, DECSTBM, IND/RI, CR/LF/BS/TAB, OSC strip.
#[derive(Clone)]
pub struct VtScreen {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<VtCell>,
    cx: usize,
    cy: usize,
    saved: (usize, usize),
    fg: u8,
    bg: u8,
    bold: bool,
    inverse: bool,
    origin: bool,
    scroll_top: usize,
    scroll_bot: usize,
    alt: bool,
    main: Vec<VtCell>,
    main_cx: usize,
    main_cy: usize,
    utf8: Vec<u8>,
    esc: Esc,
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

#[derive(Clone)]
enum Esc {
    Ground,
    Esc,
    Csi(String),
    Osc(String),
    Charset,
}

impl VtScreen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(2);
        Self {
            cols,
            rows,
            cells: vec![VtCell::default(); cols * rows],
            cx: 0,
            cy: 0,
            saved: (0, 0),
            fg: 7,
            bg: 0,
            bold: false,
            inverse: false,
            origin: false,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            alt: false,
            main: Vec::new(),
            main_cx: 0,
            main_cy: 0,
            utf8: Vec::new(),
            esc: Esc::Ground,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut next = vec![VtCell::default(); cols * rows];
        let copy_c = self.cols.min(cols);
        let copy_r = self.rows.min(rows);
        for y in 0..copy_r {
            for x in 0..copy_c {
                next[y * cols + x] = self.cells[y * self.cols + x];
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.scroll_bot = rows.saturating_sub(1);
        self.scroll_top = self.scroll_top.min(self.scroll_bot);
        // Allow cx == cols (pending wrap). Never pin at last column.
        if self.cx > cols {
            self.cx = cols;
        }
        self.cy = self.cy.min(rows.saturating_sub(1));
    }

    pub fn cell(&self, x: usize, y: usize) -> VtCell {
        self.cells.get(y * self.cols + x).copied().unwrap_or_default()
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match &mut self.esc {
            Esc::Osc(buf) => {
                if b == 0x07 || (b == b'\\' && buf.ends_with('\u{1b}')) {
                    self.esc = Esc::Ground;
                } else {
                    buf.push(b as char);
                    if buf.len() > 1024 {
                        self.esc = Esc::Ground;
                    }
                }
                return;
            }
            Esc::Csi(buf) => {
                if (0x40..=0x7e).contains(&b) {
                    let params = std::mem::take(buf);
                    self.esc = Esc::Ground;
                    self.csi(&params, b as char);
                } else {
                    buf.push(b as char);
                    if buf.len() > 64 {
                        self.esc = Esc::Ground;
                    }
                }
                return;
            }
            Esc::Esc => {
                self.esc = Esc::Ground;
                match b {
                    b'[' => {
                        self.esc = Esc::Csi(String::new());
                        return;
                    }
                    b']' => {
                        self.esc = Esc::Osc(String::new());
                        return;
                    }
                    b'(' | b')' | b'*' | b'+' => {
                        self.esc = Esc::Charset;
                        return;
                    }
                    b'7' => self.saved = (self.cx, self.cy),
                    b'8' => {
                        self.cx = self.saved.0.min(self.cols.saturating_sub(1));
                        self.cy = self.saved.1.min(self.rows.saturating_sub(1));
                    }
                    b'M' => self.ri(),
                    b'D' => self.index(),
                    b'E' => {
                        self.cx = 0;
                        self.index();
                    }
                    b'c' => {
                        let cols = self.cols;
                        let rows = self.rows;
                        *self = Self::new(cols, rows);
                    }
                    _ => {}
                }
                return;
            }
            Esc::Charset => {
                self.esc = Esc::Ground;
                return;
            }
            Esc::Ground => {}
        }

        match b {
            0x1b => {
                self.esc = Esc::Esc;
                return;
            }
            0x08 => {
                self.cx = self.cx.saturating_sub(1);
                return;
            }
            0x09 => {
                self.cx = ((self.cx / 8) + 1) * 8;
                if self.cx >= self.cols {
                    self.cx = 0;
                    self.index();
                }
                return;
            }
            0x0a | 0x0b | 0x0c => {
                self.index();
                return;
            }
            0x0d => {
                self.cx = 0;
                return;
            }
            0x07 => return,
            b if b < 0x20 => return,
            _ => {}
        }

        self.utf8.push(b);
        match std::str::from_utf8(&self.utf8) {
            Ok(s) => {
                if let Some(ch) = s.chars().next() {
                    self.put(ch);
                }
                self.utf8.clear();
            }
            Err(e) if e.error_len().is_some() => {
                self.put('�');
                self.utf8.clear();
            }
            Err(_) => {
                if self.utf8.len() > 4 {
                    self.utf8.clear();
                }
            }
        }
    }

    fn put(&mut self, ch: char) {
        // Wrap only when writing past the last column. Clamping on resize used
        // to pin cx at cols-1, so the next glyph overwrote that cell then
        // jumped to 0,0 — typed `ls` stacked on the prompt.
        if self.cx >= self.cols {
            self.cx = 0;
            self.index();
        }
        let i = self.cy * self.cols + self.cx;
        if i < self.cells.len() {
            self.cells[i] = VtCell {
                ch,
                fg: self.fg,
                bg: self.bg,
                bold: self.bold,
                inverse: self.inverse,
            };
        }
        self.cx = self.cx.saturating_add(1);
    }

    fn index(&mut self) {
        if self.cy == self.scroll_bot {
            self.scroll_up();
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn ri(&mut self) {
        if self.cy == self.scroll_top {
            self.scroll_down();
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    fn scroll_up(&mut self) {
        let cols = self.cols;
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        if bot <= top {
            return;
        }
        for y in top..bot {
            for x in 0..cols {
                self.cells[y * cols + x] = self.cells[(y + 1) * cols + x];
            }
        }
        for x in 0..cols {
            self.cells[bot * cols + x] = VtCell::default();
        }
    }

    fn scroll_down(&mut self) {
        let cols = self.cols;
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        if bot <= top {
            return;
        }
        for y in (top + 1..=bot).rev() {
            for x in 0..cols {
                self.cells[y * cols + x] = self.cells[(y - 1) * cols + x];
            }
        }
        for x in 0..cols {
            self.cells[top * cols + x] = VtCell::default();
        }
    }

    fn csi(&mut self, params: &str, cmd: char) {
        let priv_p = params.starts_with('?');
        let body = params.trim_start_matches(['?', '>', '=']);
        let nums: Vec<i32> = if body.is_empty() {
            Vec::new()
        } else {
            body.split(';')
                .map(|p| p.parse().unwrap_or(0))
                .collect()
        };
        let n = |i: usize, d: i32| nums.get(i).copied().filter(|v| *v > 0).unwrap_or(d) as usize;
        match (priv_p, cmd) {
            (true, 'h' | 'l') => {
                let on = cmd == 'h';
                for p in &nums {
                    if *p == 1049 || *p == 47 || *p == 1047 {
                        self.set_alt(on);
                    }
                }
            }
            (false, 'A') => self.cy = self.cy.saturating_sub(n(0, 1)),
            (false, 'B') => self.cy = (self.cy + n(0, 1)).min(self.rows.saturating_sub(1)),
            (false, 'C') => self.cx = (self.cx + n(0, 1)).min(self.cols.saturating_sub(1)),
            (false, 'D') => self.cx = self.cx.saturating_sub(n(0, 1)),
            (false, 'G') => self.cx = n(0, 1).saturating_sub(1).min(self.cols.saturating_sub(1)),
            (false, 'd') => self.cy = n(0, 1).saturating_sub(1).min(self.rows.saturating_sub(1)),
            (false, 'H' | 'f') => {
                let y = n(0, 1).saturating_sub(1);
                let x = n(1, 1).saturating_sub(1);
                self.cy = y.min(self.rows.saturating_sub(1));
                self.cx = x.min(self.cols.saturating_sub(1));
            }
            (false, 'J') => self.ed(nums.first().copied().unwrap_or(0)),
            (false, 'K') => self.el(nums.first().copied().unwrap_or(0)),
            (false, 'm') => self.sgr(&nums),
            (false, 'r') => {
                let top = n(0, 1).saturating_sub(1);
                let bot = if nums.len() > 1 {
                    n(1, 1).saturating_sub(1)
                } else {
                    self.rows.saturating_sub(1)
                };
                self.scroll_top = top.min(self.rows.saturating_sub(1));
                self.scroll_bot = bot.max(self.scroll_top).min(self.rows.saturating_sub(1));
            }
            (false, 's') => self.saved = (self.cx, self.cy),
            (false, 'u') => {
                self.cx = self.saved.0.min(self.cols.saturating_sub(1));
                self.cy = self.saved.1.min(self.rows.saturating_sub(1));
            }
            (false, 'L') => {
                for _ in 0..n(0, 1) {
                    self.scroll_down();
                }
            }
            (false, 'M') => {
                for _ in 0..n(0, 1) {
                    self.scroll_up();
                }
            }
            _ => {}
        }
        let _ = self.origin;
    }

    fn set_alt(&mut self, on: bool) {
        if on == self.alt {
            return;
        }
        if on {
            self.main = self.cells.clone();
            self.main_cx = self.cx;
            self.main_cy = self.cy;
            self.cells = vec![VtCell::default(); self.cols * self.rows];
            self.cx = 0;
            self.cy = 0;
            self.alt = true;
        } else {
            if self.main.len() == self.cells.len() {
                self.cells = std::mem::take(&mut self.main);
            } else {
                self.cells = vec![VtCell::default(); self.cols * self.rows];
            }
            self.cx = self.main_cx.min(self.cols.saturating_sub(1));
            self.cy = self.main_cy.min(self.rows.saturating_sub(1));
            self.alt = false;
        }
    }

    fn ed(&mut self, mode: i32) {
        match mode {
            1 => {
                for i in 0..=(self.cy * self.cols + self.cx).min(self.cells.len().saturating_sub(1)) {
                    self.cells[i] = VtCell::default();
                }
            }
            2 | 3 => {
                self.cells.fill(VtCell::default());
                if mode == 2 {
                    self.cx = 0;
                    self.cy = 0;
                }
            }
            _ => {
                let start = self.cy * self.cols + self.cx;
                for i in start..self.cells.len() {
                    self.cells[i] = VtCell::default();
                }
            }
        }
    }

    fn el(&mut self, mode: i32) {
        let row = self.cy * self.cols;
        match mode {
            1 => {
                for x in 0..=self.cx.min(self.cols.saturating_sub(1)) {
                    self.cells[row + x] = VtCell::default();
                }
            }
            2 => {
                for x in 0..self.cols {
                    self.cells[row + x] = VtCell::default();
                }
            }
            _ => {
                for x in self.cx..self.cols {
                    self.cells[row + x] = VtCell::default();
                }
            }
        }
    }

    fn sgr(&mut self, nums: &[i32]) {
        if nums.is_empty() {
            self.fg = 7;
            self.bg = 0;
            self.bold = false;
            self.inverse = false;
            return;
        }
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => {
                    self.fg = 7;
                    self.bg = 0;
                    self.bold = false;
                    self.inverse = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                7 => self.inverse = true,
                27 => self.inverse = false,
                n @ 30..=37 => self.fg = (n - 30) as u8,
                39 => self.fg = 7,
                n @ 40..=47 => self.bg = (n - 40) as u8,
                49 => self.bg = 0,
                n @ 90..=97 => self.fg = (n - 90 + 8) as u8,
                n @ 100..=107 => self.bg = (n - 100 + 8) as u8,
                38 | 48 => {
                    let is_fg = nums[i] == 38;
                    if i + 2 < nums.len() && nums[i + 1] == 5 {
                        let idx = nums[i + 2].clamp(0, 255) as u8;
                        if is_fg {
                            self.fg = idx;
                        } else {
                            self.bg = idx;
                        }
                        i += 2;
                    } else if i + 4 < nums.len() && nums[i + 1] == 2 {
                        // Approximate 24-bit as 256-color cube.
                        let r = nums[i + 2].clamp(0, 255) as u8;
                        let g = nums[i + 3].clamp(0, 255) as u8;
                        let b = nums[i + 4].clamp(0, 255) as u8;
                        let idx = rgb_to_256(r, g, b);
                        if is_fg {
                            self.fg = idx;
                        } else {
                            self.bg = idx;
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
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
}
