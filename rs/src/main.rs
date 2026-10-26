//! syncbrowse: find the same photo (upscales, recolors, copies, raw + JPEG) by dHash and compare the versions
//! side by side in its own window.
//!
//!   syncbrowse <folder>   index the folder (recursive) and open the window
//!   syncbrowse            only open; more folders are scanned via "Choose folder"
//!
//! Every scanned tree lands in one index (%LOCALAPPDATA%\syncbrowse, movable in the window).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // one window: no console, no browser tab
mod index;
mod pic;
mod win;

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("../../page.html");

/// Real path; mapped drives become their share (I:\x -> \\10.1.3.5\foto\x) so every route finds the same entries.
fn canon(p: &str) -> String {
    let Ok(c) = std::fs::canonicalize(p) else { return p.to_string() };
    let s = c.to_string_lossy().into_owned();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").map_or(s.clone(), str::to_string),
    }
}

fn main() {
    win::attach_console(); // started from a terminal: messages still show there
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() > 1 || args.first().is_some_and(|a| !Path::new(a).is_dir()) {
        eprintln!("usage: syncbrowse [folder]");
        std::process::exit(2);
    }
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let start = canon(args.first().unwrap_or(&cwd));
    index::load();
    if !args.is_empty() {
        index::start_scan(start.clone());
    }
    let server = Server::http("127.0.0.1:0").expect("cannot listen on 127.0.0.1"); // free port, no clash
    let port = server.server_addr().to_ip().expect("ip address").port();
    let url = format!("http://127.0.0.1:{port}");
    println!("Index {} (every folder scanned so far): {} pictures. {url}", index::db_file(), index::count());
    std::thread::spawn(move || {
        for rq in server.incoming_requests() {
            let start = start.clone();
            std::thread::spawn(move || route(rq, &start, port));
        }
    });
    if let Err(e) = app_window(&url) {
        eprintln!("no app window ({e}), using the browser instead");
        win::open(&url);
        loop {
            std::thread::park();
        }
    }
}

/// The page in a WebView2 window (the Edge engine that ships with Windows); closing it ends the program.
fn app_window(url: &str) -> Result<(), String> {
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::platform::windows::{IconExtWindows, WindowBuilderExtWindows};
    use tao::window::{Icon, Theme};
    let events = EventLoop::new();
    let icon = || Icon::from_resource(1, None).ok(); // put into the exe by build.rs
    let window = tao::window::WindowBuilder::new().with_title("syncbrowse").with_maximized(true)
        .with_theme(Some(Theme::Dark)).with_window_icon(icon()).with_taskbar_icon(icon())
        .build(&events).map_err(|e| e.to_string())?;
    // WebView2 keeps its profile here instead of a folder next to the exe
    let mut ctx = wry::WebContext::new(Some(index::app_dir().join("webview")));
    let view = wry::WebViewBuilder::new_with_web_context(&mut ctx).with_url(url).build(&window).map_err(|e| e.to_string())?;
    events.run(move |event, _, flow| {
        let _alive = (&view, &window, &ctx);
        *flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            std::process::exit(0); // every hashed picture is already committed
        }
    })
}

fn header(rq: &Request, name: &'static str) -> String {
    rq.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.to_string()).unwrap_or_default()
}

fn send(rq: Request, code: u16, body: Vec<u8>, ctype: &str) {
    let r = Response::from_data(body).with_status_code(code).with_header(Header::from_bytes("Content-Type", ctype).unwrap());
    rq.respond(r).ok(); // the browser cancelled the image while flipping fast
}

fn reply(rq: Request, code: u16, v: Value) {
    send(rq, code, v.to_string().into_bytes(), "application/json");
}

fn route(rq: Request, start: &str, port: u16) {
    // Host check blocks DNS rebinding; the custom header on POST forces a CORS preflight we never grant
    let host = header(&rq, "Host");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return reply(rq, 403, json!({"error": "forbidden"}));
    }
    let (path, query) = rq.url().split_once('?').unwrap_or((rq.url(), ""));
    let path = path.to_string();
    let q: HashMap<String, String> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let arg = |k: &str| q.get(k).cloned().unwrap_or_default();
    let p = canon(&if arg("p").is_empty() { start.to_string() } else { arg("p") });
    let tol = arg("t").parse().unwrap_or(10);
    let is_dir = Path::new(&p).is_dir();
    let img_ok = pic::is_picture(&p, index::raw_on()) && Path::new(&p).is_file();
    if *rq.method() == Method::Post {
        if header(&rq, "X-Syncbrowse") != "1" {
            return reply(rq, 403, json!({"error": "forbidden"}));
        }
        return match path.as_str() {
            "/api/delete" if img_ok => delete(rq, &p, arg("force") == "1"),
            "/api/ignore" => {
                index::save_ignore(&arg("v"));
                reply(rq, 200, json!({"ok": true}))
            }
            "/api/raw" => {
                let on = arg("v") == "1";
                index::set_raw(on);
                // raw files in the open folder get hashed right away, else their badges stay empty
                let scanning = on && is_dir && index::start_scan(p.clone());
                reply(rq, 200, json!({"ok": true, "scanning": scanning}))
            }
            "/api/db/move" => match win::pick_folder(&index::app_dir().to_string_lossy()) {
                None => reply(rq, 200, json!({"path": null})),
                Some(f) => match index::move_db(&canon(&f)) {
                    Ok((path, existed)) => reply(rq, 200, json!({"path": path, "existed": existed})),
                    Err(e) => reply(rq, 500, json!({"error": e})),
                },
            },
            "/api/db/reset" => match index::reset() {
                Ok(()) => reply(rq, 200, json!({"ok": true})),
                Err(e) => reply(rq, 500, json!({"error": e})),
            },
            "/api/open" if img_ok => {
                win::open(&p);
                reply(rq, 200, json!({"ok": true}))
            }
            "/api/pick" => match win::pick_folder(if is_dir { &p } else { start }) {
                None => reply(rq, 200, json!({"path": null})),
                Some(c) => {
                    let c = canon(&c);
                    let scanning = index::start_scan(c.clone());
                    reply(rq, 200, json!({"path": c, "scanning": scanning}))
                }
            },
            _ => reply(rq, 404, json!({"error": "not found"})),
        };
    }
    match path.as_str() {
        "/" => send(rq, 200, PAGE.as_bytes().to_vec(), "text/html; charset=utf-8"),
        "/api/status" => {
            let mut s = index::status(&p);
            s["thumbs"] = json!(THUMBS_BUSY.load(SeqCst));
            reply(rq, 200, s)
        }
        "/api/dir" if is_dir => match index::listing(&p, tol, arg("r") == "1") {
            Ok(v) => reply(rq, 200, v),
            Err(e) => reply(rq, 500, json!({"error": e})),
        },
        "/api/match" if img_ok => reply(rq, 200, index::matches(&p, tol)),
        "/api/meta" if img_ok => {
            let (tool, prompt) = pic::made_by(&p);
            reply(rq, 200, json!({"tool": tool, "prompt": prompt}))
        }
        "/thumb" if img_ok => match thumb(&p) {
            Ok(b) => send(rq, 200, b, "image/jpeg"),
            Err(_) => reply(rq, 404, json!({"error": "unreadable"})),
        },
        "/img" if img_ok => {
            let web = pic::WEB.contains(&pic::ext(&p).as_str());
            let body = if web { std::fs::read(&p).map_err(|e| e.to_string()) } else { pic::browser_jpeg(&p) };
            match body {
                Ok(b) => send(rq, 200, b, pic::mime(&p)),
                Err(e) => reply(rq, 404, json!({"error": e})),
            }
        }
        _ => reply(rq, 404, json!({"error": "not found"})),
    }
}

static THUMBS_BUSY: AtomicUsize = AtomicUsize::new(0); // shown in the status bar

/// Last 1000 thumbnails; size + mtime in the key, so an edited file gets a fresh one.
fn thumb(p: &str) -> Result<Vec<u8>, String> {
    static CACHE: Mutex<(VecDeque<String>, Option<HashMap<String, Vec<u8>>>)> = Mutex::new((VecDeque::new(), None));
    let (size, mtime) = index::stat(p).ok_or("gone")?;
    let key = format!("{p}|{size}|{mtime}");
    if let Some(b) = CACHE.lock().unwrap().1.as_ref().and_then(|m| m.get(&key)) {
        return Ok(b.clone());
    }
    THUMBS_BUSY.fetch_add(1, SeqCst);
    let b = std::panic::catch_unwind(|| pic::thumb(p));
    THUMBS_BUSY.fetch_sub(1, SeqCst);
    let b = b.map_err(|_| "decoder crashed".to_string())??;
    let mut c = CACHE.lock().unwrap();
    let (order, map) = &mut *c;
    let map = map.get_or_insert_with(HashMap::new);
    if map.insert(key.clone(), b.clone()).is_none() {
        order.push_back(key);
    }
    if order.len() > 1000 && let Some(old) = order.pop_front() {
        map.remove(&old);
    }
    Ok(b)
}

fn delete(rq: Request, p: &str, force: bool) {
    let remote = win::on_network(p);
    if remote && !force {
        return reply(rq, 200, json!({"confirm": true}));
    }
    let done = if remote { std::fs::remove_file(p).map_err(|e| e.to_string()) } else { win::trash(p) };
    match done {
        Err(e) => reply(rq, 500, json!({"error": e})),
        Ok(()) => {
            index::forget(&[p.to_string()]);
            reply(rq, 200, json!({"ok": true}))
        }
    }
}
