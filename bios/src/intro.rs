// Boot intro: when a cart is handed over, the bios UI plays an animation of the logo with a boot
// chime while the launcher mounts the cart and preloads it into the page cache, so the game
// starts faster. The launcher creates a ready file once the intro may end, and the intro ends
// once it exists (after a minimum time). What the game reads is recorded once it has started,
// and the next preload reads exactly that.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use macroquad::prelude::*;

use crate::launcher;
use crate::{SoundEffects, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Argument that plays the boot intro on its own, for previewing it
pub const INTRO_ARG: &str = "--intro";
/// Argument that prints the cached ranges of a cart, see print_cache_ranges
pub const CACHE_RANGES_ARG: &str = "--cache-ranges";
/// How much of the cart is read into the page cache while the intro plays
const PRELOAD_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const PRELOAD_CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Files up to this size (executables, libraries, engine data) are read before the larger ones
const PRELOAD_SMALL_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// How long after the game starts its reads are recorded for the next preload
const TRACE_DELAY: Duration = Duration::from_secs(60);
/// Cached ranges closer than this are merged, so they are read sequentially
const TRACE_MERGE_GAP: u64 = 1024 * 1024;
/// A larger trace is not recorded, it would hold more than what the game needs to start
const TRACE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;
const INTRO_BLACK: f64 = 0.4; // black screen before the logo when started on its own
const INTRO_ENTER: f64 = 1.0; // logo moves to the center with a zoom pulse
const INTRO_PULSE: f32 = 0.12; // extra zoom at the middle of the entrance
const INTRO_COLOR_STEP: f64 = 0.6; // seconds between colors while waiting
const INTRO_MIN: f64 = 3.0; // minimum time before the intro may end
const INTRO_MAX: f64 = 120.0; // safety limit in case the cart media hangs while preloading
const INTRO_EXIT: f64 = 0.8; // fade to black
const INTRO_READY_POLL: f64 = 0.1;
const INTRO_COLORS: [Color; 6] = [
    Color { r: 1.0, g: 0.5, b: 0.5, a: 1.0 },
    Color { r: 1.0, g: 1.0, b: 0.5, a: 1.0 },
    Color { r: 0.5, g: 1.0, b: 0.5, a: 1.0 },
    Color { r: 0.5, g: 1.0, b: 1.0, a: 1.0 },
    Color { r: 0.5, g: 0.5, b: 1.0, a: 1.0 },
    Color { r: 1.0, g: 0.5, b: 1.0, a: 1.0 },
];

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// The logo moves from where it was to the center with a zoom pulse and the boot chime, cycles
/// colors while the launcher prepares the cart and fades to black once the ready file exists
pub struct IntroAnimation {
    start: f64,
    delay: f64,
    from_pos: Vec2,
    from_alpha: f32,
    from_scale: f32,
    sound_played: bool,
    last_poll: f64,
    ready: bool,
    exit_start: Option<f64>,
}

impl IntroAnimation {
    /// `from_pos` is the top left corner of the unscaled logo when the intro starts
    pub fn new(from_pos: Vec2, from_alpha: f32, from_scale: f32, delay: f64) -> Self {
        IntroAnimation {
            start: get_time(),
            delay,
            from_pos,
            from_alpha,
            from_scale,
            sound_played: false,
            last_poll: f64::MIN,
            ready: false,
            exit_start: None,
        }
    }

    /// Draw one frame; returns true once the intro has faded out
    pub fn draw(&mut self, logo: &Texture2D, sound_effects: &SoundEffects, ready_file: &Path) -> bool {
        let elapsed = get_time() - self.start;

        if elapsed >= self.delay {
            if !self.sound_played {
                sound_effects.play_boot();
                self.sound_played = true;
            }

            let enter = ((elapsed - self.delay) / INTRO_ENTER).min(1.0) as f32;
            let eased = 1.0 - (1.0 - enter).powi(3);
            let scale = self.from_scale + (1.0 - self.from_scale) * eased
                + INTRO_PULSE * (std::f32::consts::PI * enter).sin();

            // white during the entrance, then slowly cycle through the colors
            let hold = (elapsed - self.delay - INTRO_ENTER).max(0.0) / INTRO_COLOR_STEP;
            let index = hold as usize;
            let mut color = if hold <= 0.0 {
                WHITE
            } else if index == 0 {
                lerp_color(WHITE, INTRO_COLORS[0], hold as f32)
            } else {
                lerp_color(
                    INTRO_COLORS[(index - 1) % INTRO_COLORS.len()],
                    INTRO_COLORS[index % INTRO_COLORS.len()],
                    hold.fract() as f32,
                )
            };
            color.a = self.from_alpha + (1.0 - self.from_alpha) * eased;

            let center = Vec2::new(
                ((SCREEN_WIDTH as f32 - logo.width()) / 2.0).round(),
                ((SCREEN_HEIGHT as f32 - logo.height()) / 2.0).round(),
            );
            let pos = self.from_pos + (center - self.from_pos) * eased;
            // scale around the middle of the logo
            let w = (logo.width() * scale).round();
            let h = (logo.height() * scale).round();
            let x = (pos.x + (logo.width() - w) / 2.0).round();
            let y = (pos.y + (logo.height() - h) / 2.0).round();
            draw_texture_ex(logo, x, y, color, DrawTextureParams {
                dest_size: Some(Vec2::new(w, h)),
                ..Default::default()
            });
        }

        if self.exit_start.is_none() {
            if !self.ready && elapsed - self.last_poll >= INTRO_READY_POLL {
                self.ready = ready_file.exists();
                self.last_poll = elapsed;
            }
            if elapsed >= INTRO_MIN && (self.ready || elapsed >= INTRO_MAX) {
                self.exit_start = Some(elapsed);
            }
        }

        match self.exit_start {
            Some(exit_time) => {
                let progress = ((elapsed - exit_time) / INTRO_EXIT).min(1.0) as f32;
                draw_rectangle(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32,
                    Color { r: 0.0, g: 0.0, b: 0.0, a: progress });
                progress >= 1.0
            }
            None => false,
        }
    }
}

/// Play the intro on its own, ending once `ready_file` exists; for previewing it
pub async fn run_intro(ready_file: PathBuf) {
    let logo = Texture2D::from_file_with_format(include_bytes!("../logo.png"), Some(ImageFormat::Png));
    logo.set_filter(FilterMode::Nearest);
    let sound_effects = SoundEffects::new().await;
    let center = Vec2::new(
        ((SCREEN_WIDTH as f32 - logo.width()) / 2.0).round(),
        ((SCREEN_HEIGHT as f32 - logo.height()) / 2.0).round(),
    );
    let mut intro = IntroAnimation::new(center, 0.0, 0.6, INTRO_BLACK);
    loop {
        clear_background(BLACK);
        if intro.draw(&logo, &sound_effects, &ready_file) {
            process::exit(0);
        }
        next_frame().await
    }
}

/// Created by the launcher once the cart is mounted and preloaded, so the intro may end
pub fn ready_file() -> PathBuf {
    launcher::runtime_dir().join("kazeta-intro-ready")
}

/// Split a cart's Exec line into words, honoring double and single quotes
fn exec_words(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    for c in exec.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            None => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// All regular files below `dir` with their sizes, without following symlinks
fn cart_files(dir: &Path, files: &mut Vec<(u64, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else { continue };
        if meta.is_dir() {
            cart_files(&entry.path(), files);
        } else if meta.is_file() {
            files.push((meta.len(), entry.path()));
        }
    }
}

/// Order in which the preload reads a cart:
/// 1. small files named in Exec (following a leading `cd <dir>`), usually the game executable
/// 2. other small files, which hold what games read when they start (executables, libraries,
///    engine data), shallowest first since content packs tend to be nested deeper
/// 3. large files named in Exec, such as a disc image passed to an emulator
/// 4. other large files, smallest first
fn preload_order(cart_path: &Path, cart_exec: &str) -> Vec<PathBuf> {
    let cart_path = &fs::canonicalize(cart_path).unwrap_or_else(|_| cart_path.to_path_buf());
    let mut exec_files: Vec<PathBuf> = Vec::new();
    let mut exec_dir = cart_path.to_path_buf();
    let mut words = exec_words(cart_exec).into_iter();
    while let Some(word) = words.next() {
        if word == "cd" {
            if let Some(dir) = words.next() {
                exec_dir = cart_path.join(dir);
            }
            continue;
        }
        for path in [exec_dir.join(&word), cart_path.join(&word)] {
            if let Ok(path) = fs::canonicalize(&path) {
                if path.starts_with(cart_path) && path.is_file() && !exec_files.contains(&path) {
                    exec_files.push(path);
                }
            }
        }
    }

    let mut files = Vec::new();
    cart_files(cart_path, &mut files);
    let class = |size: u64, path: &Path| {
        let in_exec = exec_files.iter().position(|exec| exec == path);
        let small = size <= PRELOAD_SMALL_FILE_BYTES;
        match (small, in_exec) {
            (true, Some(index)) => (0, index),
            (true, None) => (1, 0),
            (false, Some(index)) => (2, index),
            (false, None) => (3, 0),
        }
    };
    let depth = |path: &Path| path.components().count();
    files.sort_by(|a, b| {
        class(a.0, &a.1).cmp(&class(b.0, &b.1)).then_with(|| {
            if a.0 <= PRELOAD_SMALL_FILE_BYTES {
                depth(&a.1).cmp(&depth(&b.1)).then_with(|| a.1.cmp(&b.1))
            } else {
                a.0.cmp(&b.0)
            }
        })
    });
    files.into_iter().map(|(_, path)| path).collect()
}

/// Read up to PRELOAD_MAX_BYTES of the cart's files, in `order`; returns the bytes read
fn preload_files(order: Vec<PathBuf>, cancel: &AtomicBool) -> u64 {
    let mut budget = PRELOAD_MAX_BYTES;
    let mut buffer = vec![0u8; PRELOAD_CHUNK_SIZE];
    'files: for path in order {
        let Ok(mut file) = fs::File::open(&path) else { continue };
        loop {
            if budget == 0 || cancel.load(Ordering::Relaxed) {
                break 'files;
            }
            let len = buffer.len().min(budget as usize);
            match file.read(&mut buffer[..len]) {
                Ok(0) | Err(_) => break,
                Ok(n) => budget -= n as u64,
            }
        }
    }
    PRELOAD_MAX_BYTES - budget
}

/// A stretch of a cart file that the game read, from the trace
struct TraceRange {
    path: PathBuf,
    offset: u64,
    len: u64,
}

/// Where the trace of what a cart's game reads is kept: in its save data, so it survives reboots
fn trace_file(cart_home: &Path) -> PathBuf {
    cart_home.join(".kazeta/var/preload.trace")
}

/// Read the trace recorded for a cart, one `<file size> <offset> <length> <relative path>` line
/// per range. Ranges of files that are gone or changed size (a re-flashed cart) are skipped.
fn load_trace(cart_path: &Path, cart_home: &Path) -> Option<Vec<TraceRange>> {
    let trace = fs::read_to_string(trace_file(cart_home)).ok()?;
    let mut ranges = Vec::new();
    for line in trace.lines() {
        let mut fields = line.splitn(4, ' ');
        let (Some(size), Some(offset), Some(len), Some(path)) = (fields.next(), fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(size), Ok(offset), Ok(len)) = (size.parse::<u64>(), offset.parse::<u64>(), len.parse::<u64>()) else {
            continue;
        };
        let path = cart_path.join(path);
        if fs::metadata(&path).is_ok_and(|meta| meta.len() == size) {
            ranges.push(TraceRange { path, offset, len });
        }
    }
    Some(ranges)
}

/// Read the traced ranges in order; returns the bytes read
fn preload_trace(ranges: &[TraceRange], cancel: &AtomicBool) -> u64 {
    let mut total = 0;
    let mut buffer = vec![0u8; PRELOAD_CHUNK_SIZE];
    let mut open: Option<(&Path, fs::File)> = None;
    for range in ranges {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if open.as_ref().is_none_or(|(path, _)| *path != range.path) {
            open = fs::File::open(&range.path).ok().map(|file| (range.path.as_path(), file));
        }
        let Some((_, file)) = open.as_mut() else { continue };
        if file.seek(SeekFrom::Start(range.offset)).is_err() {
            continue;
        }
        let mut remaining = range.len;
        while remaining > 0 && !cancel.load(Ordering::Relaxed) {
            let len = buffer.len().min(remaining as usize);
            match file.read(&mut buffer[..len]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    remaining -= n as u64;
                    total += n as u64;
                }
            }
        }
    }
    total
}

/// Reads the cart into the page cache in the background while the intro plays.
/// With a trace of what the game read last time, exactly that is read and the intro waits for
/// it. Without one, a guess is read (executables and engine files first) and the intro does not
/// wait: reading more than the game needs from slow media only delays it.
pub struct Preload {
    cancel: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Preload {
    /// `ready_file` is created once the intro may end; `cart_home` is the mounted cart
    pub fn start(cart_path: &Path, cart_home: &Path, cart_exec: &str, ready_file: &Path) -> Self {
        let trace = load_trace(cart_path, cart_home);
        if trace.is_none() {
            let _ = fs::File::create(ready_file);
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let cart_path = cart_path.to_path_buf();
        let cart_exec = cart_exec.to_string();
        let ready_file = ready_file.to_path_buf();
        let handle = thread::spawn(move || {
            let start = Instant::now();
            let (bytes, kind) = match &trace {
                Some(ranges) => (preload_trace(ranges, &thread_cancel), "trace"),
                None => (preload_files(preload_order(&cart_path, &cart_exec), &thread_cancel), "guess"),
            };
            let cancelled = thread_cancel.load(Ordering::Relaxed);
            let secs = start.elapsed().as_secs_f64();
            log_event(&format!(
                "Preloaded {} MB of the cart in {:.1} s ({:.1} MB/s, {}){}",
                bytes / MB,
                secs,
                bytes as f64 / MB as f64 / secs.max(0.001),
                kind,
                if cancelled { ", stopped when the game started" } else { "" },
            ));
            if trace.is_some() && !cancelled {
                let _ = fs::File::create(&ready_file);
            }
        });
        Preload { cancel, handle: Some(handle) }
    }
}

impl Drop for Preload {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Ranges of `path` that are in the page cache, as (offset, length), with ranges closer than
/// TRACE_MERGE_GAP merged so they are read sequentially
fn cached_ranges(path: &Path, size: u64) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let Ok(file) = fs::File::open(path) else { return ranges };
    if size == 0 {
        return ranges;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let addr = unsafe {
        libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
    };
    if addr == libc::MAP_FAILED {
        return ranges;
    }
    let mut resident = vec![0u8; size.div_ceil(page) as usize];
    let result = unsafe { libc::mincore(addr as _, size as usize, resident.as_mut_ptr() as _) };
    unsafe { libc::munmap(addr, size as usize) };
    if result != 0 {
        return ranges;
    }

    for (index, flags) in resident.iter().enumerate() {
        if flags & 1 == 0 {
            continue;
        }
        let start = index as u64 * page;
        let end = (start + page).min(size);
        match ranges.last_mut() {
            Some((offset, len)) if start - (*offset + *len) <= TRACE_MERGE_GAP => *len = end - *offset,
            _ => ranges.push((start, end - start)),
        }
    }
    ranges
}

/// Print the cached ranges of every file below `dir`, one `<file size> <offset> <length>
/// <relative path>` line per range. Run as root through `kazeta-mount cache-ranges`: the kernel
/// only tells the page cache state of files the caller may write or owns (otherwise mincore
/// reports every page as cached), and cart files belong to whoever wrote the cart.
pub fn print_cache_ranges(dir: Option<&str>) -> i32 {
    let Some(dir) = dir else {
        eprintln!("Usage: kazeta {} <cart directory>", CACHE_RANGES_ARG);
        return 1;
    };
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
    let mut files = Vec::new();
    cart_files(&dir, &mut files);
    let mut out = String::new();
    for (size, path) in files {
        let Ok(relative) = path.strip_prefix(&dir) else { continue };
        for (offset, len) in cached_ranges(&path, size) {
            out.push_str(&format!("{} {} {} {}\n", size, offset, len, relative.display()));
        }
    }
    print!("{}", out);
    0
}

/// Cached ranges of the cart's files, as trace lines by relative path
fn query_cache_ranges(cart_path: &Path) -> Option<String> {
    // the kernel answers truthfully for root only, see print_cache_ranges; on other systems (for
    // testing) the files are queried directly
    if cfg!(target_os = "linux") && unsafe { libc::geteuid() } != 0 {
        let output = Command::new("pkexec")
            .args(["kazeta-mount", "cache-ranges"])
            .arg(cart_path)
            .output()
            .map_err(|e| eprintln!("Failed to run kazeta-mount: {}", e))
            .ok()?;
        if !output.status.success() {
            eprintln!("kazeta-mount cache-ranges failed: {}", output.status);
            return None;
        }
        return String::from_utf8(output.stdout).ok();
    }
    let mut out = String::new();
    let mut files = Vec::new();
    cart_files(cart_path, &mut files);
    for (size, path) in files {
        let Ok(relative) = path.strip_prefix(cart_path) else { continue };
        for (offset, len) in cached_ranges(&path, size) {
            out.push_str(&format!("{} {} {} {}\n", size, offset, len, relative.display()));
        }
    }
    Some(out)
}

/// Write down which parts of the cart are in the page cache: what the game has read so far
fn record_trace(cart_path: &Path, cart_home: &Path, cart_exec: &str) {
    let cart_path = &fs::canonicalize(cart_path).unwrap_or_else(|_| cart_path.to_path_buf());
    let Some(ranges) = query_cache_ranges(cart_path) else { return };
    let mut by_path: HashMap<&str, Vec<&str>> = HashMap::new();
    for line in ranges.lines() {
        if let Some(path) = line.splitn(4, ' ').nth(3) {
            by_path.entry(path).or_default().push(line);
        }
    }

    let mut trace = String::new();
    let mut total = 0;
    let mut count = 0;
    // same priority as the guess, so the files needed first are read first next time
    for path in preload_order(cart_path, cart_exec) {
        let Ok(relative) = path.strip_prefix(cart_path) else { continue };
        let Some(lines) = by_path.get(relative.to_string_lossy().as_ref()) else { continue };
        for line in lines {
            total += line.split(' ').nth(2).and_then(|len| len.parse::<u64>().ok()).unwrap_or(0);
            count += 1;
            trace.push_str(line);
            trace.push('\n');
        }
    }
    if total > TRACE_MAX_BYTES {
        eprintln!("Not recording a preload trace of {} MB, over the limit", total / MB);
        return;
    }

    let file = trace_file(cart_home);
    let tmp = file.with_extension("trace.tmp");
    match fs::write(&tmp, trace).and_then(|_| fs::rename(&tmp, &file)) {
        Ok(_) => log_event(&format!("Recorded preload trace: {} MB in {} ranges", total / MB, count)),
        Err(e) => eprintln!("Failed to write {}: {}", file.display(), e),
    }
}

/// Log a step of starting a cart with the time, to measure where the boot time goes
pub fn log_event(event: &str) {
    eprintln!("[{}] {}", Local::now().format("%H:%M:%S%.3f"), event);
}

/// Records what the game read, TRACE_DELAY after it started, for the next preload. Only the
/// first game of a cart since boot is recorded, since the page cache then holds nothing else of
/// it. The trace also holds what the preload itself read, so it can grow but does not shrink;
/// deleting .kazeta/var/preload.trace from the save data starts it over.
pub struct TraceRecorder {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TraceRecorder {
    pub fn start(cart_path: &Path, cart_home: &Path, cart_id: &str, cart_exec: &str) -> Option<Self> {
        if cart_id.trim().is_empty() {
            return None;
        }
        // in the runtime directory, which is emptied on every boot
        let marker = launcher::runtime_dir().join(format!("kazeta-traced-{}", cart_id.trim().replace('/', "_")));
        if marker.exists() {
            return None;
        }
        let _ = fs::File::create(&marker);

        let (stop, rx) = mpsc::channel::<()>();
        let cart_path = cart_path.to_path_buf();
        let cart_home = cart_home.to_path_buf();
        let cart_exec = cart_exec.to_string();
        let handle = thread::spawn(move || {
            // nothing is recorded when the game exits before the delay
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(TRACE_DELAY) {
                record_trace(&cart_path, &cart_home, &cart_exec);
            }
        });
        Some(TraceRecorder { stop: Some(stop), handle: Some(handle) })
    }
}

impl Drop for TraceRecorder {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
