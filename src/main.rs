use anyhow::{Context, Result};
use clap::{ArgAction, Parser as ClapParser};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::HashMap,
    env,
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

const DEFAULT_MIN_PANE_WIDTH: usize = 19;
const DEFAULT_MIN_PANE_HEIGHT: usize = 10;
const SWAP_ADJACENCY_TOLERANCE: i32 = 1;
const MAX_OUTPUT: usize = 1_000_000;
const PTY_CHANNEL_BOUND: usize = 256;

#[derive(ClapParser, Debug)]
#[command(name = "mmux", version = "mmux 1.0", disable_version_flag = true, about = "A lightweight terminal multiplexer.", after_help = "Keys:\n  Ctrl+V                Split vertical\n  Ctrl+H                Split horizontal\n  Ctrl+Space            Toggle zoom\n  Ctrl+B                Toggle broadcast mode\n  Ctrl+Arrows           Navigate between panes\n  Ctrl+Shift+Arrows     Resize pane boundary\n  Alt+Shift+Arrows      Swap with adjacent pane")]
struct Args {
    #[arg(short = 's', long = "shell", default_value_t = default_shell())]
    shell: String,
    #[arg(short = 'n', long = "no-instructions")]
    no_instructions: bool,
    #[arg(short = 'd', long = "debug")]
    debug: bool,
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    version: Option<bool>,
    #[arg(long = "min-width", default_value_t = DEFAULT_MIN_PANE_WIDTH)]
    min_width: usize,
    #[arg(long = "min-height", default_value_t = DEFAULT_MIN_PANE_HEIGHT)]
    min_height: usize,
}

fn default_shell() -> String {
    env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction { Up, Down, Left, Right }

struct Pane {
    id: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    pid: u32,
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Vec<u8>,
    rx: Receiver<ReadMessage>,
    screen: vt100::Parser,
}

enum ReadMessage { Data(Vec<u8>), Eof }

impl Pane {
    fn new(id: usize, command: &str, x: usize, y: usize, width: usize, height: usize) -> Result<Self> {
        let rows = height.saturating_sub(2).max(1).min(u16::MAX as usize) as u16;
        let cols = width.saturating_sub(2).max(1).min(u16::MAX as usize) as u16;
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut cmd = CommandBuilder::new(command);
        cmd.env("TERM", env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()));
        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id().unwrap_or(0) as u32;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;

        // Use a sync_channel to introduce backpressure and avoid unbounded OOM memory growth.
        let (tx, rx) = mpsc::sync_channel(PTY_CHANNEL_BOUND);
        thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => { let _ = tx.send(ReadMessage::Eof); break; }
                    Ok(n) => { if tx.send(ReadMessage::Data(buf[..n].to_vec())).is_err() { break; } }
                    Err(_) => { let _ = tx.send(ReadMessage::Eof); break; }
                }
            }
        });
        let mut pane = Self {
            id, x, y, width, height, pid, master, writer, child,
            output: Vec::new(), rx,
            screen: vt100::Parser::new(rows, cols, 0),
        };
        pane.notify_window_size();
        Ok(pane)
    }

    fn cols(&self) -> u16 { self.width.saturating_sub(2).max(1).min(u16::MAX as usize) as u16 }
    fn rows(&self) -> u16 { self.height.saturating_sub(2).max(1).min(u16::MAX as usize) as u16 }

    fn rebuild_screen(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        self.screen = vt100::Parser::new(rows, cols, 0);
        self.screen.process(&self.output);
    }

    fn notify_window_size(&mut self) {
        let _ = self.master.resize(PtySize { rows: self.rows(), cols: self.cols(), pixel_width: 0, pixel_height: 0 });
    }

    fn refresh_window_size(&mut self) {
        let (rows, cols) = self.screen.screen().size();
        if rows != self.rows() || cols != self.cols() {
            self.rebuild_screen();
            self.notify_window_size();
        }
    }

    fn resize(&mut self, x: usize, y: usize, width: usize, height: usize) {
        self.x = x; self.y = y; self.width = width; self.height = height;
        self.refresh_window_size();
    }

    fn feed_data(&mut self, data: &[u8]) {
        self.output.extend_from_slice(data);
        if self.output.len() > MAX_OUTPUT {
            // Truncate to half of MAX_OUTPUT to prevent tight-loop re-parsing CPU spikes
            let target_len = MAX_OUTPUT / 2;
            let start = self.output.len() - target_len;
            self.output.drain(..start);
            self.rebuild_screen();
        } else {
            self.screen.process(data);
        }
    }

    fn write(&mut self, bytes: &[u8]) { let _ = self.writer.write_all(bytes); let _ = self.writer.flush(); }

    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait(); // Prevent zombie processes and reclaim PID table entry
        let _ = self.writer.flush();
    }

    fn swap_terminal_state(&mut self, other: &mut Pane) {
        std::mem::swap(&mut self.pid, &mut other.pid);
        std::mem::swap(&mut self.master, &mut other.master);
        std::mem::swap(&mut self.writer, &mut other.writer);
        std::mem::swap(&mut self.child, &mut other.child);
        std::mem::swap(&mut self.output, &mut other.output);
        std::mem::swap(&mut self.rx, &mut other.rx);
        self.rebuild_screen();
        other.rebuild_screen();
        self.notify_window_size();
        other.notify_window_size();
    }
}

struct LayoutNode {
    pane: Option<usize>,
    left: Option<Box<LayoutNode>>,
    right: Option<Box<LayoutNode>>,
    is_vertical: bool,
    split_ratio: f64,
}

impl LayoutNode {
    fn leaf(pane: usize) -> Box<Self> {
        Box::new(Self { pane: Some(pane), left: None, right: None, is_vertical: false, split_ratio: 0.5 })
    }
    fn is_leaf(&self) -> bool { self.pane.is_some() }
}

struct App {
    shell: String,
    no_instructions: bool,
    debug_mode: bool,
    min_pane_width: usize,
    min_pane_height: usize,
    panes: Vec<Pane>,
    active_index: usize,
    root: Box<LayoutNode>,
    next_id: usize,
    is_zoomed: bool,
    broadcast_mode: bool,
    last_key_code: Option<String>,
    screen_width: usize,
    screen_height: usize,
    full_redraw: bool,
}

impl App {
    fn new(args: Args) -> Result<Self> {
        terminal::enable_raw_mode()?;
        let (width, height) = terminal::size()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        let width = width as usize;
        let height = height as usize;
        let bottom = if args.no_instructions { 0 } else { 2 };
        let mut pane = Pane::new(0, &args.shell, 0, 0, width, height.saturating_sub(bottom).max(2))?;
        pane.rebuild_screen();
        Ok(Self {
            shell: args.shell,
            no_instructions: args.no_instructions,
            debug_mode: args.debug,
            min_pane_width: args.min_width.max(2),
            min_pane_height: args.min_height.max(2),
            panes: vec![pane], active_index: 0,
            root: LayoutNode::leaf(0), next_id: 1,
            is_zoomed: false, broadcast_mode: false, last_key_code: None,
            screen_width: width, screen_height: height,
            full_redraw: true,
        })
    }

    fn cleanup(&mut self) {
        for p in &mut self.panes { p.close(); }
        let mut out = io::stdout();
        let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }

    fn recalculate_layout(&mut self) {
        if self.panes.is_empty() { return; }
        self.full_redraw = true;
        let bottom = if self.no_instructions { 0 } else { 2 };
        let width = self.screen_width;
        let height = self.screen_height.saturating_sub(bottom).max(1);
        let mut positions = Vec::new();
        Self::apply_layout(&self.root, 0, 0, width, height, &mut positions);
        let map: HashMap<usize, (usize,usize,usize,usize)> = positions.into_iter().collect();
        for pane in &mut self.panes {
            if let Some(&(x,y,w,h)) = map.get(&pane.id) { pane.resize(x,y,w.max(2),h.max(2)); }
        }
    }

    fn apply_layout(node: &LayoutNode, x: usize, y: usize, width: usize, height: usize, out: &mut Vec<(usize,(usize,usize,usize,usize))>) {
        if let Some(id) = node.pane { out.push((id,(x,y,width.max(2),height.max(2)))); return; }
        if let (Some(left), Some(right)) = (&node.left, &node.right) {
            if node.is_vertical {
                let lw = ((width as f64) * node.split_ratio) as usize;
                Self::apply_layout(left, x, y, lw, height, out);
                Self::apply_layout(right, x + lw, y, width.saturating_sub(lw), height, out);
            } else {
                let th = ((height as f64) * node.split_ratio) as usize;
                Self::apply_layout(left, x, y, width, th, out);
                Self::apply_layout(right, x, y + th, width, height.saturating_sub(th), out);
            }
        }
    }

    fn check_min_sizes(&self, node: &LayoutNode, width: usize, height: usize) -> bool {
        if node.is_leaf() { return width >= self.min_pane_width && height >= self.min_pane_height; }
        let (Some(left), Some(right)) = (&node.left, &node.right) else { return true; };
        if node.is_vertical {
            let lw = ((width as f64) * node.split_ratio) as usize;
            self.check_min_sizes(left, lw, height) && self.check_min_sizes(right, width.saturating_sub(lw), height)
        } else {
            let th = ((height as f64) * node.split_ratio) as usize;
            self.check_min_sizes(left, width, th) && self.check_min_sizes(right, width, height.saturating_sub(th))
        }
    }

    fn toggle_maximize(&mut self) {
        if self.panes.is_empty() { return; }
        self.full_redraw = true;
        let bottom = if self.no_instructions { 0 } else { 2 };
        if !self.is_zoomed {
            let (w,h) = (self.screen_width, self.screen_height.saturating_sub(bottom));
            let p = &mut self.panes[self.active_index];
            p.resize(0,0,w,h);
            self.is_zoomed = true;
        } else { self.is_zoomed = false; self.recalculate_layout(); }
    }

    fn split(&mut self, vertical: bool) {
        if self.is_zoomed || self.panes.is_empty() { return; }
        let active_id = self.panes[self.active_index].id;
        if vertical && self.panes[self.active_index].width < self.min_pane_width * 2 { return; }
        if !vertical && self.panes[self.active_index].height < self.min_pane_height * 2 { return; }
        let new_id = self.next_id; self.next_id += 1;
        let new_pane = match Pane::new(new_id, &self.shell, 0,0,80,24) { Ok(p) => p, Err(_) => return };
        {
            let Some(node) = Self::find_layout_node_mut(&mut self.root, active_id) else { return; };
            let old = node.pane.take().unwrap();
            node.is_vertical = vertical;
            node.split_ratio = 0.5;
            node.left = Some(LayoutNode::leaf(old));
            node.right = Some(LayoutNode::leaf(new_id));
        }
        self.panes.insert(self.active_index + 1, new_pane);
        self.active_index += 1;
        self.full_redraw = true;
        self.recalculate_layout();
    }

    fn find_layout_node_mut<'a>(node: &'a mut LayoutNode, pane_id: usize) -> Option<&'a mut LayoutNode> {
        if node.pane == Some(pane_id) { return Some(node); }
        if let Some(left) = node.left.as_deref_mut() { if let Some(found) = Self::find_layout_node_mut(left, pane_id) { return Some(found); } }
        if let Some(right) = node.right.as_deref_mut() { if let Some(found) = Self::find_layout_node_mut(right, pane_id) { return Some(found); } }
        None
    }

    fn remove_pane(&mut self, pane_id: usize) -> bool {
        let Some(index) = self.panes.iter().position(|p| p.id == pane_id) else { return false; };
        self.panes[index].close();
        self.panes.remove(index);
        self.full_redraw = true;
        if self.panes.is_empty() { return true; }
        if self.is_zoomed { self.is_zoomed = false; }
        Self::collapse_layout(&mut self.root, pane_id);
        self.active_index = self.active_index.min(self.panes.len()-1);
        self.recalculate_layout();
        false
    }

    fn collapse_layout(node: &mut Box<LayoutNode>, target: usize) -> bool {
        if node.is_leaf() { return node.pane == Some(target); }
        if node.left.as_ref().is_some_and(|n| n.pane == Some(target)) {
            let sibling = node.right.take().unwrap(); *node = sibling; return true;
        }
        if node.right.as_ref().is_some_and(|n| n.pane == Some(target)) {
            let sibling = node.left.take().unwrap(); *node = sibling; return true;
        }
        if let Some(left) = node.left.as_mut() { if Self::collapse_layout(left, target) { return true; } }
        if let Some(right) = node.right.as_mut() { if Self::collapse_layout(right, target) { return true; } }
        false
    }

    fn navigate(&mut self, direction: Direction) {
        if self.panes.is_empty() || self.is_zoomed { return; }
        let cur = &self.panes[self.active_index];
        let (cl, cr, ct, cb) = (cur.x as i32, (cur.x+cur.width) as i32, cur.y as i32, (cur.y+cur.height) as i32);
        let mut best = self.active_index; let mut min_dist = i32::MAX;
        for (i,p) in self.panes.iter().enumerate() {
            if i == self.active_index { continue; }
            let (pl, pr, pt, pb) = (p.x as i32,(p.x+p.width) as i32,p.y as i32,(p.y+p.height) as i32);
            let mut d = i32::MAX;
            match direction {
                Direction::Left if pr <= cl + 1 && (cb.min(pb)-ct.max(pt)) > 0 => d = cl-pr,
                Direction::Right if pl >= cr - 1 && (cb.min(pb)-ct.max(pt)) > 0 => d = pl-cr,
                Direction::Up if pb <= ct + 1 && (cr.min(pr)-cl.max(pl)) > 0 => d = ct-pb,
                Direction::Down if pt >= cb - 1 && (cr.min(pr)-cl.max(pl)) > 0 => d = pt-cb,
                _ => {}
            }
            if d < min_dist { min_dist = d; best = i; }
        }
        self.active_index = best;
    }

    fn find_swap_pane(&self, direction: Direction) -> Option<usize> {
        if self.panes.is_empty() { return None; }
        let a = &self.panes[self.active_index];
        let (al,ar,at,ab) = (a.x as i32,(a.x+a.width) as i32,a.y as i32,(a.y+a.height) as i32);
        let mut best=None; let mut best_dist=i32::MAX;
        for (i,p) in self.panes.iter().enumerate() {
            if i == self.active_index { continue; }
            let (pl,pr,pt,pb)=(p.x as i32,(p.x+p.width) as i32,p.y as i32,(p.y+p.height) as i32);
            let adjacent=match direction {
                Direction::Left => (pr-al).abs() <= SWAP_ADJACENCY_TOLERANCE,
                Direction::Right => (pl-ar).abs() <= SWAP_ADJACENCY_TOLERANCE,
                Direction::Up => (pb-at).abs() <= SWAP_ADJACENCY_TOLERANCE,
                Direction::Down => (pt-ab).abs() <= SWAP_ADJACENCY_TOLERANCE,
            };
            if !adjacent { continue; }
            let dist=match direction {
                Direction::Left|Direction::Right => { if ab.min(pb)-at.max(pt)<=0 {continue;} ((pt+pb)-(at+ab)).abs()/2 },
                Direction::Up|Direction::Down => { if ar.min(pr)-al.max(pl)<=0 {continue;} ((pl+pr)-(al+ar)).abs()/2 },
            };
            if dist<best_dist { best_dist=dist; best=Some(i); }
        }
        best
    }

    fn swap_active(&mut self, direction: Direction) {
        if self.panes.is_empty() || self.is_zoomed { return; }
        let Some(other) = self.find_swap_pane(direction) else { return; };
        let current = self.active_index;
        if current < other {
            let (a,b)=self.panes.split_at_mut(other);
            a[current].swap_terminal_state(&mut b[0]);
        } else {
            let (a,b)=self.panes.split_at_mut(current);
            b[0].swap_terminal_state(&mut a[other]);
        }
        self.active_index=other;
        self.full_redraw = true;
        self.recalculate_layout();
    }

    fn find_path(node: &LayoutNode, pane_id: usize, path: &mut Vec<bool>) -> bool {
        if node.pane == Some(pane_id) { return true; }
        if let Some(left) = node.left.as_deref() {
            path.push(false);
            if Self::find_path(left, pane_id, path) { return true; }
            path.pop();
        }
        if let Some(right) = node.right.as_deref() {
            path.push(true);
            if Self::find_path(right, pane_id, path) { return true; }
            path.pop();
        }
        false
    }

    fn node_at_path_mut<'a>(mut node: &'a mut LayoutNode, path: &[bool]) -> &'a mut LayoutNode {
        for &right in path {
            node = if right {
                node.right.as_deref_mut().unwrap()
            } else {
                node.left.as_deref_mut().unwrap()
            };
        }
        node
    }

    fn resize_active(&mut self, direction: Direction) {
        if self.panes.is_empty() || self.is_zoomed { return; }
        let id = self.panes[self.active_index].id;
        let mut path = Vec::new();
        if !Self::find_path(&self.root, id, &mut path) { return; }

        for depth in (0..path.len()).rev() {
            let ancestor_path = &path[..depth];
            let matches = {
                let node = Self::node_at_path_mut(&mut self.root, ancestor_path);
                match direction {
                    Direction::Left | Direction::Right => node.is_vertical,
                    Direction::Up | Direction::Down => !node.is_vertical,
                }
            };
            if !matches { continue; }

            let original = Self::node_at_path_mut(&mut self.root, ancestor_path).split_ratio;
            {
                let node = Self::node_at_path_mut(&mut self.root, ancestor_path);
                let delta = match direction {
                    Direction::Left | Direction::Up => -0.05,
                    Direction::Right | Direction::Down => 0.05,
                };
                node.split_ratio = (node.split_ratio + delta).clamp(0.01, 0.99);
            }

            let bottom = if self.no_instructions { 0 } else { 2 };
            let valid = self.check_min_sizes(
                &self.root,
                self.screen_width,
                self.screen_height.saturating_sub(bottom).max(1),
            );
            if !valid {
                Self::node_at_path_mut(&mut self.root, ancestor_path).split_ratio = original;
            } else {
                self.full_redraw = true;
            }
            break;
        }
        self.recalculate_layout();
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.debug_mode { self.last_key_code=Some(format!("{:?}", key)); }
        let ctrl=key.modifiers.contains(KeyModifiers::CONTROL);
        let alt=key.modifiers.contains(KeyModifiers::ALT);
        let shift=key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('v') if ctrl && !alt => { self.split(true); true }
            KeyCode::Char('h') if ctrl && !alt => { self.split(false); true }
            KeyCode::Char('b') if ctrl && !alt => { self.broadcast_mode = !self.broadcast_mode; self.full_redraw = true; true }
            KeyCode::Char(' ') if ctrl => { self.toggle_maximize(); true }
            KeyCode::Backspace | KeyCode::Delete => { if let Some(p)=self.panes.get_mut(self.active_index){p.write(b"\x7f");} true }
            KeyCode::Up|KeyCode::Down|KeyCode::Left|KeyCode::Right => {
                let d=match key.code {KeyCode::Up=>Direction::Up,KeyCode::Down=>Direction::Down,KeyCode::Left=>Direction::Left,_=>Direction::Right};
                if ctrl && shift { self.resize_active(d); true }
                else if alt && shift { self.swap_active(d); true }
                else if ctrl { self.navigate(d); true }
                else { self.send_key(key); true }
            }
            KeyCode::Esc => { if let Some(p)=self.panes.get_mut(self.active_index){p.write(b"\x1b");} true }
            _ => { self.send_key(key); true }
        }
    }

    fn send_key(&mut self, key: KeyEvent) {
        let mut bytes=Vec::new();
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if matches!(key.code, KeyCode::Up|KeyCode::Down|KeyCode::Left|KeyCode::Right) {
            let final_byte = match key.code { KeyCode::Up=>'A', KeyCode::Down=>'B', KeyCode::Right=>'C', _=>'D' };
            let modifier = 1 + if shift {2} else {0} + if alt {2} else {0} + if ctrl {4} else {0};
            if modifier == 1 {
                bytes.extend_from_slice(format!("\x1b[{}", final_byte).as_bytes());
            } else {
                bytes.extend_from_slice(format!("\x1b[1;{}{}", modifier, final_byte).as_bytes());
            }
        } else {
            if alt { bytes.push(0x1b); }
            match key.code {
                KeyCode::Char(c) => {
                    if ctrl {
                        let u=c.to_ascii_lowercase() as u8;
                        if (b'a'..=b'z').contains(&u) { bytes.push(u-b'a'+1); }
                        else if u == b'@' { bytes.push(0); }
                        else { bytes.push(u & 0x1f); }
                    } else { let mut s=[0u8;4]; bytes.extend_from_slice(c.encode_utf8(&mut s).as_bytes()); }
                }
                KeyCode::Enter => bytes.extend_from_slice(b"\r"),
                KeyCode::Tab => bytes.extend_from_slice(b"\t"),
                KeyCode::Backspace|KeyCode::Delete => bytes.push(0x7f),
                KeyCode::Esc => bytes.push(0x1b),
                KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
                KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
                KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
                KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
                KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
                KeyCode::F(n) if (1..=12).contains(&n) => {
                    let seq=[b"\x1bOP".as_slice(),b"\x1bOQ".as_slice(),b"\x1bOR".as_slice(),b"\x1bOS".as_slice()];
                    if n<=4 { bytes.extend_from_slice(seq[(n-1) as usize]); }
                    else { let code=10+n; bytes.extend_from_slice(format!("\x1b[{}~",code).as_bytes()); }
                }
                _ => {}
            }
        }
        if !bytes.is_empty() {
            if self.broadcast_mode {
                for p in &mut self.panes {
                    p.write(&bytes);
                }
            } else {
                if let Some(p) = self.panes.get_mut(self.active_index) {
                    p.write(&bytes);
                }
            }
        }
    }

    fn drain_output(&mut self) -> (bool, bool) {
        let mut dead=Vec::new();
        let mut dirty = false;
        for p in &mut self.panes {
            loop {
                match p.rx.try_recv() {
                    Ok(ReadMessage::Data(d))=>{ p.feed_data(&d); dirty = true; },
                    Ok(ReadMessage::Eof)=>{ dead.push(p.id); dirty = true; break; },
                    Err(mpsc::TryRecvError::Empty)=>break,
                    Err(mpsc::TryRecvError::Disconnected)=>{ dead.push(p.id); dirty = true; break; },
                }
            }
        }
        for id in dead { if self.remove_pane(id) { return (false, dirty); } }
        (true, dirty)
    }

    fn draw(&mut self) -> Result<()> {
        let mut out=io::stdout();
        queue!(out, cursor::Hide)?;

        if self.full_redraw {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
            self.full_redraw = false;
        }

        let bottom=if self.no_instructions{0}else{2};
        let visible:Vec<usize>=if self.is_zoomed{vec![self.active_index]}else{(0..self.panes.len()).collect()};
        for i in visible {
            if i>=self.panes.len(){continue;}
            self.draw_pane(&mut out,i,bottom)?;
        }
        if !self.no_instructions { self.draw_footer(&mut out)?; }

        // Place the real hardware cursor at the active pane's cursor position
        if let Some(p)=self.panes.get(self.active_index) {
            let (cy,cx)=p.screen.screen().cursor_position();
            let x=p.x+1+cx as usize; let y=p.y+1+cy as usize;
            let maxx=p.x+p.width.saturating_sub(2); let maxy=p.y+p.height.saturating_sub(2);
            if x<=maxx && y<=maxy && x<self.screen_width && y<self.screen_height.saturating_sub(bottom) {
                queue!(out,cursor::MoveTo(x as u16,y as u16),cursor::Show)?;
            } else { queue!(out,cursor::Hide)?; }
        }
        out.flush()?; Ok(())
    }

    fn draw_pane(&self,out:&mut io::Stdout,i:usize,bottom:usize)->Result<()> {
        let p=&self.panes[i]; let active=i==self.active_index || self.broadcast_mode;
        let attr=if active{Attribute::Bold}else{Attribute::NormalIntensity};
        let border_fg=if active{CtColor::Yellow}else{CtColor::White};
        queue!(out,SetForegroundColor(border_fg),SetAttribute(attr))?;
        let right=p.x+p.width.saturating_sub(1); let bottom_y=p.y+p.height.saturating_sub(1);
        let maxy=self.screen_height.saturating_sub(bottom);
        for x in p.x..=right.min(self.screen_width.saturating_sub(1)) {
            if p.y<self.screen_height {queue!(out,cursor::MoveTo(x as u16,p.y as u16),Print(if x==p.x{'┌'}else if x==right{'┐'}else{'─'}))?;}
            if bottom_y<maxy {queue!(out,cursor::MoveTo(x as u16,bottom_y as u16),Print(if x==p.x{'└'}else if x==right{'┘'}else{'─'}))?;}
        }
        for y in p.y..=bottom_y.min(maxy.saturating_sub(1)) {
            if p.x<self.screen_width {queue!(out,cursor::MoveTo(p.x as u16,y as u16),Print(if y==p.y{'┌'}else if y==bottom_y{'└'}else{'│'}))?;}
            if right<self.screen_width {queue!(out,cursor::MoveTo(right as u16,y as u16),Print(if y==p.y{'┐'}else if y==bottom_y{'┘'}else{'│'}))?;}
        }

        let screen = p.screen.screen();
        let (rows, cols) = screen.size();
        let (cursor_r, cursor_c) = screen.cursor_position();

        for r in 0..rows {
            let sy = p.y + 1 + r as usize;
            if sy >= p.y + p.height.saturating_sub(1) || sy >= maxy { break; }

            queue!(out, cursor::MoveTo((p.x + 1) as u16, sy as u16))?;

            let mut prev_fg = None;
            let mut prev_bg = None;
            let mut prev_bold = false;
            let mut prev_under = false;
            let mut prev_rev = false;
            let mut prev_ital = false;
            let mut prev_dim = false;

            for c in 0..cols {
                let sx = p.x + 1 + c as usize;
                if sx >= p.x + p.width.saturating_sub(1) || sx >= self.screen_width { break; }

                // If broadcast mode is active, simulate a cursor visual indicator on each pane's cursor cell
                let is_simulated_cursor = self.broadcast_mode && r == cursor_r && c == cursor_c;

                if let Some(cell) = screen.cell(r, c) {
                    let fg = to_ct_color(cell.fgcolor());
                    let bg = to_ct_color(cell.bgcolor());
                    let rev = cell.inverse() || is_simulated_cursor;

                    let needs_reset = (prev_bold && !cell.bold()) || (prev_under && !cell.underline()) ||
                                      (prev_rev && !rev) || (prev_ital && !cell.italic()) ||
                                      (prev_dim && !cell.dim());

                    if needs_reset {
                        queue!(out, SetAttribute(Attribute::Reset))?;
                        prev_fg = None; prev_bg = None;
                        prev_bold = false; prev_under = false; prev_rev = false; prev_ital = false; prev_dim = false;
                    }

                    if Some(fg) != prev_fg { queue!(out, SetForegroundColor(fg))?; prev_fg = Some(fg); }
                    if Some(bg) != prev_bg { queue!(out, SetBackgroundColor(bg))?; prev_bg = Some(bg); }

                    if cell.bold() && !prev_bold { queue!(out, SetAttribute(Attribute::Bold))?; prev_bold = true; }
                    if cell.underline() && !prev_under { queue!(out, SetAttribute(Attribute::Underlined))?; prev_under = true; }
                    if rev && !prev_rev { queue!(out, SetAttribute(Attribute::Reverse))?; prev_rev = true; }
                    else if !rev && prev_rev { queue!(out, SetAttribute(Attribute::NoReverse))?; prev_rev = false; }
                    if cell.italic() && !prev_ital { queue!(out, SetAttribute(Attribute::Italic))?; prev_ital = true; }
                    if cell.dim() && !prev_dim { queue!(out, SetAttribute(Attribute::Dim))?; prev_dim = true; }

                    let ch = cell.contents();
                    if ch.is_empty() { queue!(out, Print(' '))?; } else { queue!(out, Print(ch))?; }
                } else {
                    if is_simulated_cursor {
                        if !prev_rev { queue!(out, SetAttribute(Attribute::Reverse))?; prev_rev = true; }
                        queue!(out, Print(' '))?;
                    } else {
                        if prev_fg != Some(CtColor::Reset) || prev_bg != Some(CtColor::Reset) || prev_bold || prev_under || prev_rev || prev_ital || prev_dim {
                            queue!(out, SetAttribute(Attribute::Reset), SetForegroundColor(CtColor::Reset), SetBackgroundColor(CtColor::Reset))?;
                            prev_fg = Some(CtColor::Reset); prev_bg = Some(CtColor::Reset);
                            prev_bold = false; prev_under = false; prev_rev = false; prev_ital = false; prev_dim = false;
                        }
                        queue!(out, Print(' '))?;
                    }
                }
            }
            queue!(out, SetAttribute(Attribute::Reset), SetForegroundColor(border_fg))?;
        }

        if active {
            let zoom=if self.is_zoomed{" [ ZOOMED ] "}else{""}; let pid=format!(" [ PID: {} ] ",p.pid);
            let state_label = if self.broadcast_mode { " [ BROADCAST ] " } else { " [ ACTIVE ] " };
            let long=format!("{}{}{}",zoom,pid,state_label); let short=format!("{}{}[*] ",zoom,pid);
            let title=if p.width>long.chars().count()+6{long}else{short};
            if p.width>=title.chars().count()+2 { let tx=p.x+(p.width-title.chars().count())/2; if p.x<tx && tx+title.chars().count()<p.x+p.width {queue!(out,cursor::MoveTo(tx as u16,p.y as u16),SetAttribute(Attribute::Reverse),Print(title),SetAttribute(Attribute::Reset))?;} }
        }
        queue!(out,SetAttribute(Attribute::Reset),SetBackgroundColor(CtColor::Reset),SetForegroundColor(CtColor::Reset))?;
        Ok(())
    }

    fn draw_footer(&self,out:&mut io::Stdout)->Result<()> {
        let mut m1 = if self.broadcast_mode {
            " [ BROADCAST MODE ACTIVE ] - [Ctrl+B]:Toggle Broadcast ".to_string()
        } else {
            " [Ctrl+V]:Split V | [Ctrl+H]:Split H | [Ctrl+Space]:Maximize | [Ctrl+B]:Broadcast ".to_string()
        };
        let m2=" [Ctrl+Shift+Arrows]:Resize | [Alt+Shift+Arrows]:Swap ".to_string();
        if self.debug_mode { if let Some(k)=&self.last_key_code { let tag=format!("  [key:{}]",k); let avail=self.screen_width.saturating_sub(tag.chars().count()); m1=m1.chars().take(avail).collect(); m1.push_str(&" ".repeat(avail.saturating_sub(m1.chars().count()))); m1.push_str(&tag); } }
        let row1=self.screen_height.saturating_sub(2); let row2=self.screen_height.saturating_sub(1);
        queue!(out,SetAttribute(Attribute::Reverse),cursor::MoveTo(0,row1 as u16),Print(pad_to(&m1,self.screen_width)),cursor::MoveTo(0,row2 as u16),Print(pad_to(&m2,self.screen_width)),SetAttribute(Attribute::Reset))?;
        Ok(())
    }

    fn run(&mut self)->Result<()> {
        self.recalculate_layout();
        self.draw()?;
        loop {
            let (keep_running, mut dirty) = self.drain_output();
            if !keep_running { break; }

            if event::poll(Duration::from_millis(20))? {
                while event::poll(Duration::from_millis(0))? {
                    match event::read()? {
                        Event::Resize(w,h)=>{
                            self.screen_width=w as usize; self.screen_height=h as usize;
                            if self.is_zoomed { let bottom=if self.no_instructions{0}else{2}; let p=&mut self.panes[self.active_index]; p.resize(0,0,w as usize,(h as usize).saturating_sub(bottom)); }
                            else {self.recalculate_layout();}
                            self.full_redraw = true;
                            dirty = true;
                        }
                        Event::Key(k)=>{ self.handle_key(k); dirty = true; }
                        _=>{}
                    }
                }
            }
            if dirty { self.draw()?; }
        }
        Ok(())
    }
}

fn pad_to(s:&str,n:usize)->String { let mut out=s.chars().take(n).collect::<String>(); if out.chars().count()<n{out.push_str(&" ".repeat(n-out.chars().count()));} out }

fn to_ct_color(c: vt100::Color)->CtColor {
    match c { vt100::Color::Default=>CtColor::Reset, vt100::Color::Idx(i)=>CtColor::AnsiValue(i), vt100::Color::Rgb(r,g,b)=>CtColor::Rgb{r,g,b} }
}

fn main()->Result<()> {
    let args=Args::parse();
    let mut app=App::new(args).context("failed to initialize mmux")?;
    let result=app.run(); app.cleanup(); result
}