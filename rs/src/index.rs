//! The index: fingerprints of every scanned picture in SQLite, mirrored in memory for matching.
use crate::pic::{self, Bits};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{LazyLock, Mutex, RwLock, mpsc};
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy)]
pub struct Fp {
    pub w: u32,
    pub h: u32,
    pub bits: Bits,
}

#[derive(Default)]
pub struct Scan {
    pub running: bool,
    pub folder: String,
    pub found: usize,
    pub done: usize,
    pub total: usize,
}

// Its own file name: the Python version's syncbrowse.db holds Pillow hashes, a few bits off from these
const DB_NAME: &str = "syncbrowse-index.db";
static DB_FILE: RwLock<String> = RwLock::new(String::new());
static CON: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let path = std::fs::read_to_string(config_file()).ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok()?["db"].as_str().map(str::to_string))
        .unwrap_or_else(|| app_dir().join(DB_NAME).to_string_lossy().into_owned());
    *DB_FILE.write().unwrap() = path.clone();
    Mutex::new(open(&path).unwrap_or_else(|e| panic!("cannot open the index {path}: {e}")))
});

/// %LOCALAPPDATA%\syncbrowse: index, its location setting and the window's browser profile, nothing next to the exe.
pub fn app_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("syncbrowse"))
        .or_else(|| Some(std::env::current_exe().ok()?.parent()?.to_path_buf()))
        .unwrap_or_default()
}

fn config_file() -> PathBuf {
    app_dir().join("config.json")
}

/// A missing file starts an empty index.
fn open(path: &str) -> rusqlite::Result<Connection> {
    if let Some(d) = Path::new(path).parent() {
        std::fs::create_dir_all(d).ok();
    }
    let con = Connection::open(path)?;
    // WAL + one commit per picture: closing the window loses nothing already hashed
    con.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS fp (path TEXT PRIMARY KEY, size INT, mtime REAL, w INT, h INT, hash TEXT);
        CREATE TABLE IF NOT EXISTS setting (key TEXT PRIMARY KEY, value TEXT);")?;
    Ok(con)
}

pub fn db_file() -> String {
    LazyLock::force(&CON);
    DB_FILE.read().unwrap().clone()
}

fn idle() -> Result<(), String> {
    if SCAN.lock().unwrap().running { Err("A scan is running, try again when it is done.".into()) } else { Ok(()) }
}

/// Index into `folder`: moved there, or the index already in that folder is used instead.
pub fn move_db(folder: &str) -> Result<(String, bool), String> {
    idle()?;
    let (old, target) = (db_file(), Path::new(folder).join(DB_NAME).to_string_lossy().into_owned());
    if target.eq_ignore_ascii_case(&old) {
        return Ok((old, false));
    }
    let existed = Path::new(&target).exists();
    {
        let mut con = CON.lock().unwrap();
        if !existed {
            con.execute("VACUUM INTO ?", [&target]).map_err(|e| e.to_string())?; // a clean copy, WAL included
        }
        *con = open(&target).map_err(|e| e.to_string())?; // closes the old file
    }
    *DB_FILE.write().unwrap() = target.clone();
    std::fs::create_dir_all(app_dir()).ok();
    std::fs::write(config_file(), json!({"db": target}).to_string()).map_err(|e| e.to_string())?;
    if existed {
        load();
    } else {
        for end in ["", "-wal", "-shm"] {
            std::fs::remove_file(format!("{old}{end}")).ok();
        }
    }
    Ok((target, existed))
}

/// Empty the index; pictures stay untouched, settings stay.
pub fn reset() -> Result<(), String> {
    idle()?;
    CON.lock().unwrap().execute_batch("DELETE FROM fp; VACUUM;").map_err(|e| e.to_string())?;
    HASHES.write().unwrap().clear();
    Ok(())
}
static HASHES: LazyLock<RwLock<HashMap<String, Fp>>> = LazyLock::new(Default::default);
static IGNORE: RwLock<Vec<String>> = RwLock::new(Vec::new()); // lower-case folder patterns from the "Ignore" field
static RAW: AtomicBool = AtomicBool::new(false);
pub static SCAN: Mutex<Scan> = Mutex::new(Scan { running: false, folder: String::new(), found: 0, done: 0, total: 0 });

fn setting(key: &str) -> Option<String> {
    CON.lock().unwrap().query_row("SELECT value FROM setting WHERE key = ?", [key], |r| r.get(0)).ok()
}

fn set_setting(key: &str, value: &str) {
    CON.lock().unwrap().execute("INSERT OR REPLACE INTO setting VALUES (?, ?)", [key, value]).ok();
}

pub fn load() {
    let mut fresh = HashMap::new();
    {
        let con = CON.lock().unwrap();
        let mut st = con.prepare("SELECT path, w, h, hash FROM fp WHERE hash IS NOT NULL").unwrap();
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?))).unwrap();
        for (p, w, h, x) in rows.flatten() {
            if let Some(bits) = pic::unhex(&x) {
                fresh.insert(p, Fp { w, h, bits });
            }
        }
    }
    *HASHES.write().unwrap() = fresh;
    set_ignore(&setting("ignore").unwrap_or_default());
    RAW.store(setting("raw").as_deref() == Some("1"), SeqCst);
}

pub fn count() -> usize {
    HASHES.read().unwrap().len()
}

pub fn raw_on() -> bool {
    RAW.load(SeqCst)
}

/// `*` and `?` wildcards, like Python's fnmatch for the patterns people type here.
fn glob(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            (i, j) = (i + 1, j + 1);
        } else if i < p.len() && p[i] == '*' {
            (star, mark, i) = (Some(i), j, i + 1);
        } else if let Some(st) = star {
            (i, mark) = (st + 1, mark + 1);
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// A folder of the path matches an ignore pattern: a name with wildcards (.coral, *cache*) or a chain (.coral/cache).
pub fn ignored(folder: &str) -> bool {
    let lower = folder.to_lowercase();
    let parts: Vec<&str> = lower.split(['\\', '/']).collect();
    IGNORE.read().unwrap().iter().any(|pat| {
        let chain: Vec<&str> = pat.split('/').collect();
        parts.len() >= chain.len()
            && (0..=parts.len() - chain.len()).any(|i| chain.iter().enumerate().all(|(k, c)| glob(c, parts[i + k])))
    })
}

fn set_ignore(text: &str) {
    *IGNORE.write().unwrap() = text.replace('\n', ",").split(',')
        .map(|s| s.trim().replace('\\', "/").trim_matches('/').to_lowercase())
        .filter(|s| !s.is_empty()).collect();
}

fn all_paths() -> Vec<String> {
    let con = CON.lock().unwrap();
    let mut st = con.prepare("SELECT path FROM fp").unwrap();
    st.query_map([], |r| r.get(0)).unwrap().flatten().collect()
}

pub fn save_ignore(text: &str) {
    set_ignore(text);
    set_setting("ignore", &IGNORE.read().unwrap().join(", "));
    // cached thumbnails would keep matching their originals
    let drop: Vec<String> = all_paths().into_iter().filter(|p| Path::new(p).parent().is_some_and(|d| ignored(&d.to_string_lossy()))).collect();
    forget(&drop);
}

pub fn set_raw(on: bool) {
    RAW.store(on, SeqCst);
    set_setting("raw", if on { "1" } else { "0" });
    if !on {
        let raws: Vec<String> = all_paths().into_iter().filter(|p| pic::RAW.contains(&pic::ext(p).as_str())).collect();
        forget(&raws);
    }
}

pub fn forget(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let mut con = CON.lock().unwrap();
    if let Ok(tx) = con.transaction() {
        if let Ok(mut st) = tx.prepare("DELETE FROM fp WHERE path = ?") {
            for p in paths {
                st.execute([p]).ok();
            }
        }
        tx.commit().ok();
    }
    let mut hashes = HASHES.write().unwrap();
    for p in paths {
        hashes.remove(p);
    }
}

fn mtime(m: &Metadata) -> f64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64())
}

pub fn stat(path: &str) -> Option<(i64, f64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len() as i64, mtime(&m)))
}

fn hash(path: &str) -> Option<Fp> {
    // a decoder panic on a broken file must not take the scan thread down
    match std::panic::catch_unwind(|| pic::fingerprint(path)) {
        Ok(Ok((w, h, bits))) => Some(Fp { w, h, bits }),
        Ok(Err(e)) => {
            eprintln!("unreadable, skipped until it changes: {path} ({e})");
            None // stored as NULL so rescans skip it
        }
        Err(_) => {
            eprintln!("unreadable, skipped until it changes: {path} (decoder crashed)");
            None
        }
    }
}

fn store(path: &str, size: i64, mtime: f64, f: Option<Fp>) {
    CON.lock().unwrap().execute("INSERT OR REPLACE INTO fp VALUES (?, ?, ?, ?, ?, ?)",
        params![path, size, mtime, f.map(|f| f.w), f.map(|f| f.h), f.map(|f| pic::hex(&f.bits))]).ok();
    if let Some(f) = f {
        HASHES.write().unwrap().insert(path.to_string(), f);
    }
}

/// Fingerprint from the index if size and mtime still match, else hashed now and stored.
pub fn get_fp(path: &str) -> Option<Fp> {
    let (size, mt) = stat(path)?;
    let row = CON.lock().unwrap().query_row("SELECT size, mtime, w, h, hash FROM fp WHERE path = ?", [path], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, Option<u32>>(2)?, r.get::<_, Option<u32>>(3)?, r.get::<_, Option<String>>(4)?))
    }).ok();
    if let Some((s, m, w, h, x)) = row && (s, m) == (size, mt) {
        return Some(Fp { w: w?, h: h?, bits: pic::unhex(&x?)? });
    }
    let f = hash(path);
    store(path, size, mt, f);
    f
}

fn with_sep(p: &str) -> String {
    if p.ends_with('\\') { p.to_string() } else { format!("{p}\\") }
}

struct ScanDone;
impl Drop for ScanDone {
    fn drop(&mut self) {
        if let Ok(mut s) = SCAN.lock() {
            s.running = false;
        }
    }
}

/// Hash every picture below folder into the index; unchanged files (size + mtime) are skipped.
fn scan(folder: String) {
    let _done = ScanDone;
    let raw = raw_on();
    let prefix = with_sep(&folder);
    *SCAN.lock().unwrap() = Scan { running: true, folder: folder.clone(), ..Default::default() };
    let known: HashMap<String, (i64, f64)> = {
        let con = CON.lock().unwrap();
        let mut st = con.prepare("SELECT path, size, mtime FROM fp").unwrap();
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))).unwrap()
            .flatten().filter(|(p, _)| p.starts_with(&prefix)).collect()
    };
    let (mut todo, mut seen, mut stack) = (Vec::new(), HashSet::new(), vec![folder.clone()]);
    while let Some(dir) = stack.pop() {
        // DirEntry metadata comes with the Windows listing: no network round trip per file
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (Ok(t), p) = (e.file_type(), e.path().to_string_lossy().into_owned()) else { continue };
            if t.is_dir() {
                if !ignored(&p) {
                    stack.push(p);
                }
            } else if pic::is_picture(&p, raw) && let Ok(m) = e.metadata() {
                let st = (m.len() as i64, mtime(&m));
                if known.get(&p) != Some(&st) {
                    todo.push((p.clone(), st.0, st.1));
                }
                seen.insert(p);
            }
        }
        SCAN.lock().unwrap().found = seen.len();
    }
    let gone: Vec<String> = known.into_keys().filter(|p| !seen.contains(p)).collect();
    forget(&gone);
    SCAN.lock().unwrap().total = todo.len();
    println!("{folder}: {} pictures, {} to hash, {} removed", seen.len(), todo.len(), gone.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..8 { // threads also hide the network latency of the shares
            let (tx, next, todo) = (tx.clone(), &next, &todo);
            s.spawn(move || {
                while let Some(t) = todo.get(next.fetch_add(1, SeqCst)) {
                    tx.send((t, hash(&t.0))).ok();
                }
            });
        }
        drop(tx);
        for (n, (t, f)) in rx.iter().enumerate() {
            store(&t.0, t.1, t.2, f);
            SCAN.lock().unwrap().done = n + 1;
        }
    });
}

pub fn start_scan(folder: String) -> bool {
    {
        let mut s = SCAN.lock().unwrap();
        if s.running {
            return false;
        }
        s.running = true;
    }
    std::thread::spawn(move || scan(folder));
    true
}

fn similar_to(path: &str, f: &Fp, tolerance: u32, items: &HashMap<String, Fp>) -> Vec<(u32, String)> {
    let ratio = f.w as f64 / f.h as f64;
    // same aspect ratio (2 %) as in dupimg.similar: a crop is a different picture and would not zoom in sync
    let mut v: Vec<(u32, String)> = items.iter().filter_map(|(p, x)| {
        let (d, r) = (pic::dist(&x.bits, &f.bits), x.w as f64 / x.h as f64);
        (p != path && d <= tolerance && (r - ratio).abs() <= 0.02 * r.max(ratio)).then(|| (d, p.clone()))
    }).collect();
    v.sort();
    v
}

/// `\\server\share\` or `C:\`
pub fn drive_root(p: &str) -> String {
    match p.strip_prefix(r"\\") {
        Some(rest) => {
            let mut it = rest.splitn(3, '\\');
            format!(r"\\{}\{}\", it.next().unwrap_or(""), it.next().unwrap_or(""))
        }
        None => p.chars().take(2).collect::<String>() + "\\",
    }
}

pub fn matches(path: &str, tolerance: u32) -> Value {
    let Some(f) = get_fp(path) else { return json!([]) };
    let near = similar_to(path, &f, tolerance, &HASHES.read().unwrap());
    let (mut found, mut gone) = (Vec::new(), Vec::new());
    for (d, p) in near {
        if Path::new(&p).is_file() {
            found.push(json!({"path": p, "dist": d}));
        } else if Path::new(&drive_root(&p)).is_dir() { // drive/share is up, so the file is really gone
            gone.push(p); // (a locked vault or offline NAS keeps its entries)
        }
    }
    forget(&gone);
    found.truncate(50);
    Value::Array(found)
}

fn sorted_lower(mut v: Vec<String>) -> Vec<String> {
    v.sort_by_key(|p| p.to_lowercase());
    v
}

fn walk(dir: &str, raw: bool, out: &mut Vec<String>) {
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() => if !ignored(&p) { dirs.push(p) },
            Ok(_) if pic::is_picture(&p, raw) => files.push(p),
            _ => {}
        }
    }
    out.extend(sorted_lower(files));
    for d in sorted_lower(dirs) {
        walk(&d, raw, out);
    }
}

/// Folder contents for the gallery; dups counts only indexed pictures, unscanned files show no badge.
pub fn listing(folder: &str, tolerance: u32, recursive: bool) -> Result<Value, String> {
    let raw = raw_on();
    let (mut dirs, mut paths) = (Vec::new(), Vec::new());
    for e in std::fs::read_dir(folder).map_err(|e| e.to_string())?.flatten() {
        let p = e.path().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() => if !ignored(&p) { dirs.push(p) },
            Ok(_) if pic::is_picture(&p, raw) => paths.push(p),
            _ => {}
        }
    }
    if recursive {
        paths.clear();
        walk(folder, raw, &mut paths);
    }
    let items = HASHES.read().unwrap();
    let (mut files, mut tiles, mut folded) = (Vec::new(), HashSet::new(), 0);
    for p in if recursive { paths } else { sorted_lower(paths) } {
        let f = items.get(&p).copied();
        let near: Vec<String> = f.map(|f| similar_to(&p, &f, tolerance, &items).into_iter().map(|x| x.1).collect()).unwrap_or_default();
        // recursive gathers original and versions from many folders: show the first one found, the others are
        // direct matches of that tile and appear in its comparison
        if recursive && near.iter().any(|q| tiles.contains(q)) {
            folded += 1;
            continue;
        }
        files.push(json!({"path": p, "w": f.map(|f| f.w), "h": f.map(|f| f.h), "dups": near.len()}));
        tiles.insert(p);
    }
    let parent = Path::new(folder).parent().map(|d| d.to_string_lossy().into_owned());
    Ok(json!({"path": folder, "parent": parent, "folded": folded, "dirs": sorted_lower(dirs), "files": files}))
}

/// The index holds every tree ever scanned: count the open folder separately.
pub fn status(folder: &str) -> Value {
    let prefix = with_sep(folder);
    let hashes = HASHES.read().unwrap();
    let s = SCAN.lock().unwrap();
    json!({"indexed": hashes.len(), "here": hashes.keys().filter(|k| k.starts_with(&prefix)).count(),
           "ignore": IGNORE.read().unwrap().join(", "), "raw": raw_on(), "db": db_file(),
           "running": s.running, "folder": s.folder, "found": s.found, "done": s.done, "total": s.total})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_patterns_match_folder_names_and_chains() {
        set_ignore(".Coral/cache, *Cache*, @eaDir");
        assert!(ignored(r"\\nas\home\Pictures\.coral\cache"));
        assert!(!ignored(r"\\nas\home\Pictures\.coral"));
        assert!(ignored(r"H:\Luminar 4 Catalog\PreviewCache\CD"));
        assert!(ignored(r"\\nas\foto\@eaDir"));
        assert!(!ignored(r"\\nas\foto\2025\12"));
        assert_eq!(drive_root(r"\\10.1.3.5\foto\2025\a.jpg"), r"\\10.1.3.5\foto\");
        assert_eq!(drive_root(r"C:\x\a.jpg"), r"C:\");
    }
}
