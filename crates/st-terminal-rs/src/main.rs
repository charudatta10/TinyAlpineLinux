//! st-terminal-rs — a small cross-platform terminal emulator.
//!
//! Architecture: a child process runs on a pseudo-terminal (ConPTY on
//! Windows, posix PTY on Unix); its output feeds a VT100 screen model with
//! scrollback; a software renderer paints the screen into a softbuffer
//! surface hosted by a winit window. All keyboard encoding happens locally,
//! so there is no need for the system TTY in stdin raw mode.

mod input;
mod pty;
mod render;
mod vt;

use std::num::NonZeroU32;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{Event, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::Window;

use crate::render::{CELL_H, CELL_W};
use crate::render::Renderer;
use crate::vt::Screen;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 25;
const DEFAULT_SCROLLBACK: usize = 5000;

enum UserEvent {
    Data(Vec<u8>),
    Exited,
}

struct Args {
    shell: Vec<String>,
    cols: u16,
    rows: u16,
    scrollback: usize,
    smoke: bool,
}

fn parse_args() -> Args {
    let mut shell = Vec::new();
    let mut cols = DEFAULT_COLS;
    let mut rows = DEFAULT_ROWS;
    let mut scrollback = DEFAULT_SCROLLBACK;
    let mut smoke = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!(
                    "st-terminal-rs {VERSION} — a tiny cross-platform terminal\n\n\
                     USAGE: st-terminal-rs [-c cols] [-r rows] [shell [args...]]\n\n\
                     Default shell: $SHELL (Unix) / %%COMSPEC%% (Windows)."
                );
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("st-terminal-rs {VERSION}");
                std::process::exit(0);
            }
            "--smoke" => smoke = true,
            "-c" => cols = it.next().and_then(|v| v.parse().ok()).unwrap_or(cols),
            "-r" => rows = it.next().and_then(|v| v.parse().ok()).unwrap_or(rows),
            "-e" => {
                shell.extend(it.by_ref());
                break;
            }
            other => {
                shell.push(other.to_string());
                break;
            }
        }
    }
    if shell.is_empty() && !smoke {
        shell = default_shell();
    }
    Args {
        shell,
        cols,
        rows,
        scrollback,
        smoke,
    }
}

fn default_shell() -> Vec<String> {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        vec![comspec, "/Q".into()]
    }
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        vec![shell, "-l".into()]
    }
}

fn shell_env() -> Vec<(String, String)> {
    vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("TERM_PROGRAM".to_string(), "st-terminal-rs".to_string()),
    ]
}

fn main() {
    let args = parse_args();
    if args.smoke {
        std::process::exit(smoke(&args));
    }
    if let Err(e) = run_gui(args) {
        eprintln!("st-terminal-rs: {e}");
        std::process::exit(1);
    }
}

// -------------------------------------------------------------- GUI

fn run_gui(args: Args) -> std::io::Result<()> {
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event()
        .build()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let mut app = App::new(args, proxy);

    event_loop
        .run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Wait);
            match event {
                Event::UserEvent(UserEvent::Data(bytes)) => {
                    if let Some(screen) = &mut app.screen {
                        screen.feed(&bytes);
                        screen.reset_view();
                    }
                    if let Some(w) = &app.window {
                        let _ = w.request_redraw();
                    }
                }
                Event::UserEvent(UserEvent::Exited) => {
                    if let Some(screen) = &mut app.screen {
                        screen.feed(b"\r\n[process exited]\r\n");
                    }
                    if let Some(w) = &app.window {
                        let _ = w.request_redraw();
                    }
                }
                Event::Resumed => {
                    if app.window.is_none() {
                        app.init_window(elwt);
                    }
                }
                Event::WindowEvent { window_id, event } => {
                    if let Some(w) = &app.window {
                        if w.id() == window_id {
                            app.on_window_event(elwt, &event);
                        }
                    }
                }
                _ => {}
            }
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

struct App {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    renderer: Option<Renderer>,
    screen: Option<Screen>,
    mods: ModifiersState,
    cols: u16,
    rows: u16,
    shell: Vec<String>,
    started: bool,
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    ctl: Option<Arc<Mutex<Box<dyn pty::Ctl>>>>,
    proxy: EventLoopProxy<UserEvent>,
}

impl App {
    fn new(args: Args, proxy: EventLoopProxy<UserEvent>) -> Self {
        App {
            window: None,
            surface: None,
            renderer: None,
            screen: None,
            mods: ModifiersState::empty(),
            cols: args.cols,
            rows: args.rows,
            shell: args.shell,
            started: false,
            writer_tx: None,
            ctl: None,
            proxy,
        }
    }

    fn on_window_event(&mut self, elwt: &ActiveEventLoop, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(ctl) = &self.ctl {
                    if let Ok(mut c) = ctl.lock() {
                        let _ = c.kill();
                    }
                }
                elwt.exit();
            }
            WindowEvent::Resized(size) => self.on_resize(*size),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let app_cursor = self.screen.as_ref().map(|s| s.app_cursor_keys).unwrap_or(false);
                if let Some(bytes) = input::encode_key(event, self.mods, app_cursor) {
                    if let Some(tx) = &self.writer_tx {
                        let _ = tx.send(bytes);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        if *y > 0.0 {
                            (*y as isize).max(1)
                        } else {
                            (*y as isize).min(-1)
                        }
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        let lines = (p.y / CELL_H as f64) as isize;
                        lines.signum()
                    }
                };
                if let Some(screen) = &mut self.screen {
                    screen.scroll_view(lines);
                }
                if let Some(w) = &self.window {
                    let _ = w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn init_window(&mut self, elwt: &ActiveEventLoop) {
        let size = LogicalSize::new(
            self.cols as f64 * CELL_W as f64,
            self.rows as f64 * CELL_H as f64,
        );
        let window = match elwt.create_window(
            Window::default_attributes()
                .with_title("st-terminal-rs")
                .with_inner_size(size),
        ) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("st-terminal-rs: create window: {e}");
                elwt.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("st-terminal-rs: softbuffer context: {e}");
                elwt.exit();
                return;
            }
        };
        let mut surface = softbuffer::Surface::new(&context, window.clone()).expect("surface");
        // softbuffer requires an explicit size before the first buffer_mut().
        let size = window.inner_size();
        if let (Some(w), Some(h)) = (
            NonZeroU32::new(size.width.max(1)),
            NonZeroU32::new(size.height.max(1)),
        ) {
            let _ = surface.resize(w, h);
        }

        self.surface = Some(surface);
        self.renderer = Some(Renderer::new(CELL_W, CELL_H));
        self.screen = Some(Screen::new(
            self.cols as usize,
            self.rows as usize,
            DEFAULT_SCROLLBACK,
        ));
        self.window = Some(window);

        self.spawn_child();
        // First draw.
        self.draw();
    }

    fn spawn_child(&mut self) {
        self.started = true;
        let argv = self.shell.clone();
        match pty::spawn(&argv, &shell_env(), None, self.cols, self.rows) {
            Ok(pty::Pty {
                mut reader,
                mut writer,
                ctl,
            }) => {
                self.ctl = Some(ctl.clone());

                // Writer thread: consumes keystrokes from the UI thread.
                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                self.writer_tx = Some(tx);
                std::thread::spawn(move || {
                    while let Ok(bytes) = rx.recv() {
                        let _ = writer.write(&bytes);
                    }
                });

                // Reader thread: pumps pty output into the UI thread and
                // detects child exit.
                let proxy = self.proxy.clone();
                let ctl_thread = ctl.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16384];
                    let mut idle: u32 = 0;
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                idle += 1;
                                if idle % 64 == 0 {
                                    let running = ctl_thread
                                        .lock()
                                        .map(|mut c| c.running())
                                        .unwrap_or(false);
                                    if !running {
                                        let _ = proxy.send_event(UserEvent::Exited);
                                        return;
                                    }
                                }
                                std::thread::sleep(Duration::from_millis(8));
                            }
                            Ok(n) => {
                                idle = 0;
                                if proxy.send_event(UserEvent::Data(buf[..n].to_vec())).is_err() {
                                    return;
                                }
                            }
                            Err(_) => {
                                let _ = proxy.send_event(UserEvent::Exited);
                                return;
                            }
                        }
                    }
                });
            }
            Err(e) => {
                if let Some(screen) = &mut self.screen {
                    let msg = format!("\r\nfailed to spawn shell: {e}\r\n");
                    screen.feed(msg.as_bytes());
                }
            }
        }
    }

    fn on_resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let cols = ((size.width / CELL_W).max(1)) as u16;
        let rows = ((size.height / CELL_H).max(1)) as u16;
        if let Some(surface) = &mut self.surface {
            if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                let _ = surface.resize(w, h);
            }
        }
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            if let Some(screen) = &mut self.screen {
                screen.resize(cols as usize, rows as usize);
            }
            if let Some(ctl) = &self.ctl {
                if let Ok(mut c) = ctl.lock() {
                    let _ = c.resize(cols, rows);
                }
            }
        }
        self.draw();
    }

    fn draw(&mut self) {
        let (surface, renderer, screen) = match (&mut self.surface, &mut self.renderer, &self.screen) {
            (Some(s), Some(r), Some(c)) => (s, r, c),
            _ => return,
        };
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let w = buf.width().get() as usize;
        let h = buf.height().get() as usize;
        renderer.render(&mut buf, w, h, screen);
        buf.present().ok();
    }
}

// -------------------------------------------------------------- smoke test

/// Headless test: spawn a child on the pty, pump its output into the screen
/// model, and verify the expected marker text appears.
fn smoke(args: &Args) -> i32 {
    let argv = if args.shell.is_empty() {
        // Default smoke invocation: drive our own rsh shell.
        vec![
            "rsh".to_string(),
            "-c".to_string(),
            "echo smoke-ok-$((6*7)); printf '\\033[32mgreen\\033[0m done\\r\\n'".to_string(),
        ]
    } else {
        args.shell.clone()
    };

    // Make sure the sibling binaries (rsh) resolve via PATH.
    let mut envs = shell_env();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let old = std::env::var("PATH").unwrap_or_default();
            envs.push((
                "PATH".to_string(),
                format!("{};{old}", dir.display()),
            ));
        }
    }
    let pty = match pty::spawn(&argv, &envs, None, 80, 24) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("smoke: spawn failed: {e}");
            return 1;
        }
    };
    let mut screen = Screen::new(80, 24, 100);
    let mut reader = pty.reader;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(n) => {
                screen.feed(&buf[..n]);
            }
            Err(e) => {
                eprintln!("smoke: read error: {e}");
                return 1;
            }
        }
        let text: String = (0..screen.rows)
            .filter_map(|y| screen.viewport_line(y))
            .flat_map(|line| line.iter().map(|c| c.ch))
            .collect();
        if text.contains("green done") {
            if text.contains("smoke-ok-42") {
                println!("smoke: OK");
                return 0;
            }
            eprintln!("smoke: saw color line but shell arithmetic output missing");
            eprintln!("{text}");
            return 1;
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    eprintln!("smoke: timed out waiting for output");
    let text: String = (0..screen.rows)
        .filter_map(|y| screen.viewport_line(y))
        .flat_map(|line| line.iter().map(|c| c.ch))
        .collect();
    eprintln!("{text}");
    1
}
