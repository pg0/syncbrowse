r"""syncbrowse: gleiche Fotos (Upscales, Edits, Kopien) per pHash finden und nebeneinander vergleichen.

  syncbrowse .    aktuellen Ordner rekursiv indexieren und im Browser oeffnen
  syncbrowse      nur oeffnen; weitere Ordner im Browser ueber "Ordner waehlen" scannen

Alle gescannten Baeume landen in einer gemeinsamen DB (syncbrowse.db neben dem Skript).
"""
import ctypes
import fnmatch
import functools
import io
import json
import mimetypes
import os
import sqlite3
import sys
import threading
import warnings
import webbrowser
from concurrent.futures import ThreadPoolExecutor
from ctypes import wintypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from PIL import Image, ImageOps

try:
    import pillow_heif
    pillow_heif.register_heif_opener()
except ImportError:
    pass

# Pillow warns above 89 MP and refuses above twice its limit, which stored big upscales as unreadable. This reads
# the user's own archive; the cap only stops a single decode from needing several GB.
Image.MAX_IMAGE_PIXELS = 400_000_000

# Pillow's warnings name its own source line, not the picture: remember per thread which file is being decoded
DECODING = threading.local()
warnings.filterwarnings('always', module='PIL')  # the default shows each warning once, hiding every later file
warnings.showwarning = lambda message, category, *_: print(
    f'{category.__name__}: {message} | {getattr(DECODING, "path", "?")}', file=sys.stderr, flush=True)

# fingerprint comes from the dupimg CLI so both tools judge "same picture" identically
sys.path.insert(0, os.path.join(os.path.expanduser('~'), 'bin'))
from dupimg import fingerprint  # 16x16 dHash (256 bit) + width/height

EXTS = {'.jpg', '.jpeg', '.png', '.webp', '.bmp', '.gif', '.tif', '.tiff', '.heic', '.heif'}
WEB = {'.jpg', '.jpeg', '.png', '.webp', '.bmp', '.gif'}
DB = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'syncbrowse.db')
PORT = 0
THUMB = 480  # gallery thumbnail height: sharp while a tile is up to 480 screen pixels tall (few per row: softer)
LOCK = threading.Lock()
PICK_LOCK = threading.Lock()
START = CON = None
HASHES = {}
IGNORE = []  # lower-case folder patterns from the "Ignore" field, stored in the DB
SCAN ={'running': False, 'folder': '', 'found': 0, 'done': 0, 'total': 0}


def connect():
    con = sqlite3.connect(DB, check_same_thread=False)
    con.execute('CREATE TABLE IF NOT EXISTS fp (path TEXT PRIMARY KEY, size INT, mtime REAL, w INT, h INT, hash TEXT)')
    con.execute('CREATE TABLE IF NOT EXISTS setting (key TEXT PRIMARY KEY, value TEXT)')
    return con


def ignored(folder):
    """A folder of the path matches an ignore pattern: a name with wildcards (.coral, *cache*) or a chain (.coral/cache)."""
    parts = folder.lower().split(os.sep)
    for chain in (pat.split('/') for pat in IGNORE):
        if any(all(fnmatch.fnmatch(parts[i + k], c) for k, c in enumerate(chain))
               for i in range(len(parts) - len(chain) + 1)):
            return True
    return False


def set_ignore(text, save=True):
    global IGNORE
    IGNORE = [s.strip().replace('\\', '/').strip('/').lower() for s in text.replace('\n', ',').split(',') if s.strip()]
    if save:
        with LOCK:
            CON.execute('INSERT OR REPLACE INTO setting VALUES (?, ?)', ('ignore', ', '.join(IGNORE)))
            CON.commit()
            paths = [p for (p,) in CON.execute('SELECT path FROM fp')]
        forget([p for p in paths if ignored(os.path.dirname(p))])  # cached thumbnails would keep matching originals


def fp(path):
    DECODING.path = path
    try:
        w, h, bits = fingerprint(path)
    except Exception as e:
        print(f'unreadable, skipped until it changes: {path} ({e})', file=sys.stderr, flush=True)
        return None, None, None  # stored as NULL so rescans skip it
    return w, h, f'{bits:064x}'


def load():
    global CON, HASHES
    CON = connect()
    HASHES = {p: (w, h, int(x, 16)) for p, w, h, x in CON.execute('SELECT path, w, h, hash FROM fp WHERE hash IS NOT NULL')}
    row = CON.execute("SELECT value FROM setting WHERE key = 'ignore'").fetchone()
    set_ignore(row[0] if row else '', save=False)


def scan(folder):
    """Hash every image below folder into the DB; unchanged files (size + mtime) are skipped."""
    folder = os.path.realpath(folder)
    prefix = os.path.join(folder, '')
    SCAN.update(running=True, folder=folder, found=0, done=0, total=0)
    try:
        with LOCK:
            known = {p: (s, m) for p, s, m in CON.execute('SELECT path, size, mtime FROM fp')
                     if p.startswith(prefix)}
        todo, seen, stack = [], set(), [folder]
        while stack:  # DirEntry.stat comes with the Windows listing: no network round trip per file
            try:
                entries = list(os.scandir(stack.pop()))
            except OSError:
                continue
            for e in entries:
                if e.is_dir(follow_symlinks=False):
                    if not ignored(e.path):
                        stack.append(e.path)
                elif os.path.splitext(e.name)[1].lower() in EXTS:
                    try:
                        st = e.stat()
                    except OSError:
                        continue
                    seen.add(e.path)
                    if known.get(e.path) != (st.st_size, st.st_mtime):
                        todo.append((e.path, st.st_size, st.st_mtime))
            SCAN['found'] = len(seen)
        gone = [p for p in known if p not in seen]
        forget(gone)
        SCAN['total'] = len(todo)
        print(f'{folder}: {len(seen)} Bilder, {len(todo)} zu hashen, {len(gone)} entfernt', flush=True)
        with ThreadPoolExecutor(8) as ex:  # PIL decodes outside the GIL; threads also hide network latency
            for i, (row, (w, h, x)) in enumerate(zip(todo, ex.map(fp, [t[0] for t in todo])), 1):
                with LOCK:
                    CON.execute('INSERT OR REPLACE INTO fp VALUES (?, ?, ?, ?, ?, ?)', (*row, w, h, x))
                    if x:
                        HASHES[row[0]] = (w, h, int(x, 16))
                    if i % 200 == 0 or i == len(todo):
                        CON.commit()
                SCAN['done'] = i
    finally:
        SCAN['running'] = False


def forget(paths):
    with LOCK:
        CON.executemany('DELETE FROM fp WHERE path = ?', [(p,) for p in paths])
        CON.commit()
        for p in paths:
            HASHES.pop(p, None)


def start_scan(folder):
    with LOCK:
        if SCAN['running']:
            return False
        SCAN['running'] = True
    threading.Thread(target=scan, args=(folder,), daemon=True).start()
    return True


def pick_folder(start):
    """Native Windows folder dialog (the same IFileDialog picker JPEGmini uses)."""
    import tkinter
    from tkinter import filedialog
    with PICK_LOCK:
        root = tkinter.Tk()
        root.withdraw()
        root.attributes('-topmost', True)  # otherwise the dialog opens behind the browser
        try:
            return filedialog.askdirectory(parent=root, initialdir=start, mustexist=True,
                                           title='syncbrowse: choose folder')
        finally:
            root.destroy()


def get_fp(path):
    st = os.stat(path)
    with LOCK:
        row = CON.execute('SELECT size, mtime, w, h, hash FROM fp WHERE path = ?', (path,)).fetchone()
    if row and row[:2] == (st.st_size, st.st_mtime):
        w, h, x = row[2:]
    else:
        w, h, x = fp(path)
        with LOCK:
            CON.execute('INSERT OR REPLACE INTO fp VALUES (?, ?, ?, ?, ?, ?)', (path, st.st_size, st.st_mtime, w, h, x))
            CON.commit()
            if x:
                HASHES[path] = (w, h, int(x, 16))
    return (w, h, int(x, 16)) if x else None


def similar_to(path, f, tolerance, items):
    w, h, q = f
    ratio = w / h
    # same aspect ratio (2 %) as in dupimg.similar: a crop is a different picture and would not zoom in sync
    return sorted((d, p) for p, (wx, hx, x) in items
                  if p != path and (d := (x ^ q).bit_count()) <= tolerance
                  and abs(wx / hx - ratio) <= 0.02 * max(wx / hx, ratio))


def matches(path, tolerance):
    f = get_fp(path)
    if not f:
        return []
    with LOCK:
        items = list(HASHES.items())
    found, gone = [], []
    for d, p in similar_to(path, f, tolerance, items):
        if os.path.isfile(p):
            found.append({'path': p, 'dist': d})
        elif os.path.isdir(os.path.splitdrive(p)[0] + os.sep):  # drive/share is up, so the file is really gone
            gone.append(p)                                      # (a locked vault or offline NAS keeps its entries)
    forget(gone)
    return found[:50]


def walk(folder):
    for d, dirs, names in os.walk(folder):
        dirs[:] = sorted((x for x in dirs if not ignored(os.path.join(d, x))), key=str.lower)  # os.walk follows this list
        yield from (os.path.join(d, n) for n in sorted(names, key=str.lower))


def listing(folder, tolerance, recursive=False):
    """Folder contents for the gallery; dups counts only indexed pictures, unscanned files show no badge."""
    entries = sorted(os.scandir(folder), key=lambda e: e.name.lower())
    with LOCK:
        items = list(HASHES.items())
    known = dict(items)
    files, tiles, folded = [], set(), 0
    for p in walk(folder) if recursive else (e.path for e in entries if e.is_file()):
        if os.path.splitext(p)[1].lower() in EXTS:
            f = known.get(p)
            near = [q for _, q in similar_to(p, f, tolerance, items)] if f else []
            # recursive gathers original and versions from many folders: show the first one found, the others are
            # direct matches of that tile and appear in its comparison
            if recursive and not tiles.isdisjoint(near):
                folded += 1
                continue
            tiles.add(p)
            files.append({'path': p, 'w': f and f[0], 'h': f and f[1], 'dups': len(near)})
    parent = os.path.dirname(folder)
    return {'path': folder, 'parent': parent if parent != folder else None, 'folded': folded,
            'dirs': [e.path for e in entries if e.is_dir() and not ignored(e.path)], 'files': files}


def flat(im):
    """RGB with transparency shown on white, as dupimg judges it; converting P straight to RGB makes Pillow warn."""
    if im.mode in ('RGBA', 'LA', 'P'):
        bg = Image.new('RGBA', im.size, 'white')
        bg.alpha_composite(im.convert('RGBA'))
        im = bg
    return im.convert('RGB')


@functools.lru_cache(maxsize=1000)
def thumb(path, size, mtime):  # size + mtime only key the cache: an edited file gets a fresh thumbnail
    DECODING.path = path
    buf = io.BytesIO()
    with Image.open(path) as im:
        im.draft('RGB', (THUMB, THUMB))  # JPEG: decode at reduced DCT scale, a 6640 px scan loads in a fraction
        im = flat(ImageOps.exif_transpose(im))
        im.thumbnail((THUMB * 3, THUMB))
        im.save(buf, 'JPEG', quality=85)
    return buf.getvalue()


@functools.lru_cache(maxsize=2000)
def made_by(path, size, mtime):
    """What produced a picture: the models in ComfyUI's embedded graph, else EXIF Software or the JPEG comment."""
    DECODING.path = path
    with Image.open(path) as im:  # header only: ComfyUI writes its text chunks before the pixel data
        info, software = im.info, im.getexif().get(0x0131)
    try:
        graph = json.loads(info.get('prompt') or '{}')
    except (TypeError, ValueError):
        graph = {}
    models, prompts = set(), []
    for node in graph.values() if isinstance(graph, dict) else []:
        for k, v in (node.get('inputs', {}) if isinstance(node, dict) else {}).items():
            if isinstance(v, str) and k in ('ckpt_name', 'unet_name', 'model_name', 'lora_name'):  # not vae/clip
                models.add(os.path.splitext(v)[0])
            elif isinstance(v, str) and k in ('prompt', 'text') and v.strip():
                prompts.append(v)
    comment = info.get('comment', b'')
    comment = (comment.decode('latin-1') if isinstance(comment, bytes) else str(comment)).strip('\0 ')
    tool = f'ComfyUI {", ".join(sorted(models))}' if models else software or comment
    return {'tool': tool, 'prompt': '\n'.join(prompts)}


class SHFILEOPSTRUCTW(ctypes.Structure):
    _fields_ = [('hwnd', wintypes.HWND), ('wFunc', wintypes.UINT), ('pFrom', wintypes.LPCWSTR),
                ('pTo', wintypes.LPCWSTR), ('fFlags', ctypes.c_uint16), ('fAnyOperationsAborted', wintypes.BOOL),
                ('hNameMappings', ctypes.c_void_p), ('lpszProgressTitle', wintypes.LPCWSTR)]


def on_network(path):
    # network shares (NFS/SMB) have no recycle bin: a "trash" there deletes for good
    return ctypes.windll.kernel32.GetDriveTypeW(os.path.splitdrive(path)[0] + '\\') == 4  # DRIVE_REMOTE


def trash(path):
    # FO_DELETE with ALLOWUNDO | NOCONFIRMATION | SILENT | NOERRORUI; pFrom needs a double NUL
    op = SHFILEOPSTRUCTW(wFunc=3, pFrom=path + '\0', fFlags=0x40 | 0x10 | 0x4 | 0x400)
    if ctypes.windll.shell32.SHFileOperationW(ctypes.byref(op)) or op.fAnyOperationsAborted or os.path.exists(path):
        raise OSError(f'Delete failed: {path}')


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, body, ctype='application/json', code=200):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        try:
            self.send_response(code)
            self.send_header('Content-Type', ctype)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except ConnectionError:  # browser cancelled the image while flipping fast
            pass

    def do_GET(self):
        self.route()

    def do_POST(self):
        self.route()

    def route(self):
        # Host check blocks DNS rebinding; the custom header on POST forces a CORS preflight we never grant
        if self.headers.get('Host') not in (f'127.0.0.1:{PORT}', f'localhost:{PORT}'):
            return self.reply({'error': 'forbidden'}, code=403)
        url = urlparse(self.path)
        q = parse_qs(url.query)
        p = os.path.realpath(q.get('p', [''])[0] or START)
        t = q.get('t', ['10'])[0]
        tol = int(t) if t.isdigit() else 10
        img_ok = os.path.splitext(p)[1].lower() in EXTS and os.path.isfile(p)
        if self.command == 'POST':
            if self.headers.get('X-Syncbrowse') != '1':
                return self.reply({'error': 'forbidden'}, code=403)
            if url.path == '/api/delete' and img_ok:
                return self.delete(p, q.get('force') == ['1'])
            if url.path == '/api/ignore':
                set_ignore(q.get('v', [''])[0])
                return self.reply({'ok': True})
            if url.path == '/api/open' and img_ok:
                os.startfile(p)  # default viewer; img_ok keeps this to existing picture files
                return self.reply({'ok': True})
            if url.path == '/api/pick':
                chosen = pick_folder(p if os.path.isdir(p) else START)
                if not chosen:
                    return self.reply({'path': None})
                chosen = os.path.realpath(chosen)
                return self.reply({'path': chosen, 'scanning': start_scan(chosen)})
            return self.reply({'error': 'not found'}, code=404)
        if url.path == '/':
            return self.reply(PAGE.encode(), 'text/html; charset=utf-8')
        if url.path == '/api/status':  # the DB holds every tree ever scanned: count the open folder separately
            prefix = os.path.join(p, '')
            with LOCK:
                here = sum(k.startswith(prefix) for k in HASHES)
            return self.reply({'indexed': len(HASHES), 'here': here, 'ignore': ', '.join(IGNORE), **SCAN})
        if url.path == '/api/dir' and os.path.isdir(p):
            try:
                return self.reply(listing(p, tol, q.get('r') == ['1']))
            except OSError as e:
                return self.reply({'error': str(e)}, code=500)
        if url.path == '/api/match' and img_ok:
            return self.reply(matches(p, tol))
        if url.path == '/api/meta' and img_ok:
            st = os.stat(p)
            try:
                return self.reply(made_by(p, st.st_size, st.st_mtime))
            except OSError:
                return self.reply({'tool': '', 'prompt': ''})
        if url.path == '/thumb' and img_ok:
            st = os.stat(p)
            try:
                return self.reply(thumb(p, st.st_size, st.st_mtime), 'image/jpeg')
            except OSError:  # PIL's UnidentifiedImageError and truncated files are OSErrors
                return self.reply({'error': 'unreadable'}, code=404)
        if url.path == '/img' and img_ok:
            if os.path.splitext(p)[1].lower() in WEB:
                with open(p, 'rb') as f:
                    return self.reply(f.read(), mimetypes.guess_type(p)[0] or 'application/octet-stream')
            buf = io.BytesIO()  # HEIC/TIFF: browsers can't render these, convert
            DECODING.path = p
            with Image.open(p) as im:
                flat(ImageOps.exif_transpose(im)).save(buf, 'JPEG', quality=95)
            return self.reply(buf.getvalue(), 'image/jpeg')
        self.reply({'error': 'not found'}, code=404)

    def delete(self, p, force):
        remote = on_network(p)
        if remote and not force:
            return self.reply({'confirm': True})
        try:
            os.remove(p) if remote else trash(p)
        except OSError as e:
            return self.reply({'error': str(e)}, code=500)
        forget([p])
        self.reply({'ok': True})


def serve(start, scan_start):
    global START, PORT
    START = os.path.realpath(start)
    load()
    if scan_start:
        start_scan(START)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)  # free port, no clash with other local servers
    PORT = server.server_address[1]
    print(f'Index (alle bisher gescannten Ordner): {len(HASHES)} Bilder. http://127.0.0.1:{PORT}  (Strg+C beendet)', flush=True)
    webbrowser.open(f'http://127.0.0.1:{PORT}')
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        # The scan's thread pool has every remaining file queued and a normal exit would wait for all of them:
        # keep what is hashed so far and leave at once.
        with LOCK:
            CON.commit()
        os._exit(0)


PAGE = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'page.html'), encoding='utf-8').read()

if __name__ == '__main__':
    if len(sys.argv) > 2 or (len(sys.argv) == 2 and not os.path.isdir(sys.argv[1])):
        sys.exit(__doc__)
    serve(sys.argv[1] if len(sys.argv) == 2 else os.getcwd(), scan_start=len(sys.argv) == 2)
