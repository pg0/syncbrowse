import io
import json
import os
import random
import warnings

from PIL import Image, ImageDraw, PngImagePlugin

import syncbrowse as sb


def photo(seed):
    rnd = random.Random(seed)
    im = Image.new('RGB', (1200, 900), 'gray')
    d = ImageDraw.Draw(im)
    for _ in range(40):
        x, y = rnd.randrange(1200), rnd.randrange(900)
        d.ellipse((x, y, x + rnd.randrange(50, 400), y + rnd.randrange(50, 400)),
                  fill=tuple(rnd.randrange(256) for _ in range(3)))
    return im


def test_scan_and_match(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(sb, 'DB', str(tmp_path / 'idx.db'))
    archive, export = tmp_path / 'archive', tmp_path / 'export'
    (archive / 'sub').mkdir(parents=True)
    export.mkdir()
    photo(1).save(archive / 'sub' / 'a.jpg', quality=95)
    photo(2).save(archive / 'b.png')
    # same photo: smaller + harder compressed, and pixel-rotated with an EXIF tag that undoes it
    photo(1).resize((600, 450)).save(export / 'a_small.jpg', quality=40)
    exif = Image.Exif()
    exif[0x0112] = 6
    photo(1).rotate(90, expand=True).save(export / 'a_rot.jpg', exif=exif)
    photo(3).save(export / 'c.jpg')

    sb.load()
    sb.scan(str(archive))
    sb.scan(str(archive))
    assert '2 Bilder, 0 zu hashen' in capsys.readouterr().out

    a, b = (os.path.realpath(archive / n) for n in ('sub/a.jpg', 'b.png'))
    for name in ('a_small.jpg', 'a_rot.jpg'):
        hits = [m['path'] for m in sb.matches(os.path.realpath(export / name), 10)]
        assert a in hits and b not in hits, (name, hits)
    assert sb.matches(os.path.realpath(export / 'c.jpg'), 10) == []

    # gallery badge: a.jpg has its two export versions, b.png none
    assert [(f['path'], f['dups']) for f in sb.listing(os.path.realpath(archive / 'sub'), 10)['files']] == [(a, 2)]
    assert [f['dups'] for f in sb.listing(os.path.realpath(archive), 10)['files']] == [0]
    assert [(f['path'], f['dups']) for f in sb.listing(os.path.realpath(archive), 10, True)['files']] == [(b, 0), (a, 2)]
    # recursive over both folders shows a.jpg once: its export versions fold into its tile
    c = os.path.realpath(export / 'c.jpg')
    r = sb.listing(os.path.realpath(tmp_path), 10, True)
    assert [(f['path'], f['dups']) for f in r['files']] == [(b, 0), (a, 2), (c, 0)] and r['folded'] == 2
    # a match deleted outside the tool leaves the index the next time it would be shown
    small = os.path.realpath(export / 'a_small.jpg')
    os.remove(small)
    assert small not in [m['path'] for m in sb.matches(a, 10)] and small not in sb.HASHES
    assert sb.connect().execute('SELECT 1 FROM fp WHERE path = ?', (small,)).fetchone() is None
    # thumbnail honours EXIF rotation: the pixel-portrait a_rot.jpg displays landscape
    assert Image.open(io.BytesIO(sb.thumb(str(export / 'a_rot.jpg'), 0, 0))).size == (640, 480)


def test_ignore_skips_and_forgets_cache_folders(tmp_path, monkeypatch):
    monkeypatch.setattr(sb, 'DB', str(tmp_path / 'idx.db'))
    monkeypatch.setattr(sb, 'IGNORE', [])
    root = tmp_path / 'r'
    (root / '.coral' / 'cache').mkdir(parents=True)
    photo(1).save(root / 'a.jpg')
    photo(1).resize((300, 225)).save(root / '.coral' / 'cache' / 'a.jpg')
    cached = os.path.realpath(root / '.coral' / 'cache' / 'a.jpg')
    sb.load()
    sb.scan(str(root))
    assert cached in sb.HASHES
    sb.set_ignore('.Coral/cache')  # case-insensitive, nested chain
    assert cached not in sb.HASHES
    sb.load()  # the list survives a restart
    sb.scan(str(root))
    assert cached not in sb.HASHES
    assert [f['path'] for f in sb.listing(os.path.realpath(root), 10, True)['files']] == [os.path.realpath(root / 'a.jpg')]


def test_made_by_reads_comfy_models_and_jpeg_comment(tmp_path):
    graph = {'1': {'class_type': 'UNETLoader', 'inputs': {'unet_name': 'qwen_turbo.safetensors'}},
             '2': {'class_type': 'VAELoader', 'inputs': {'vae_name': 'vae.safetensors'}},
             '3': {'class_type': 'TextEncode', 'inputs': {'prompt': 'colorize', 'clip': ['4', 0]}}}
    info = PngImagePlugin.PngInfo()
    info.add_text('prompt', json.dumps(graph))
    photo(5).save(tmp_path / 'c.png', pnginfo=info)
    photo(5).save(tmp_path / 'j.jpg', comment=b'Optimized by JPEGmini\0')
    assert sb.made_by(str(tmp_path / 'c.png'), 0, 0) == {'tool': 'ComfyUI qwen_turbo', 'prompt': 'colorize'}
    assert sb.made_by(str(tmp_path / 'j.jpg'), 0, 0) == {'tool': 'Optimized by JPEGmini', 'prompt': ''}


def test_unreadable_file_names_its_path(tmp_path, capsys):
    p = tmp_path / 'broken.jpg'
    p.write_bytes(b'not a jpeg')
    assert sb.fp(str(p)) == (None, None, None)
    assert str(p) in capsys.readouterr().err


def test_palette_png_with_alpha_hashes_without_warning(tmp_path):
    p = tmp_path / 'pal.png'
    photo(4).convert('P').save(p, transparency=bytes(range(0, 256, 16)))
    with warnings.catch_warnings():
        warnings.simplefilter('error')
        assert sb.fp(str(p))[2]
        assert sb.thumb(str(p), 0, 0)
