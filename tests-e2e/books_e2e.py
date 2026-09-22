"""End-to-end check of the book features against a local test server (port 8433)."""
import io, json, re, sys, zipfile, urllib.parse
import requests

B = 'http://localhost:8433'
s = requests.Session()
s.headers['Referer'] = B + '/'

def stamp(t):
    ms = int(round(t * 1000)); return '%02d:%02d:%02d,%03d' % (ms // 3600000, ms // 60000 % 60, ms // 1000 % 60, ms % 1000)
def book(n, line):
    return '\n'.join('%d\n%s --> %s\n%s\n' % (i + 1, stamp(i * 4), stamp(i * 4 + 3.5), line) for i in range(n)).encode('utf-8')
def flashes(html):
    import html as H
    return [H.unescape(re.sub(r'<[^>]+>', ' ', m)).strip() for m in re.findall(r'class="alert[^"]*"[^>]*>\s*<p>(.*?)</p>', html, re.S)]

results = []
def check(name, ok, detail=''):
    results.append(ok); print(('PASS ' if ok else 'FAIL ') + name + (' | ' + str(detail)[:230] if detail else ''))

import random
USER = 'reader%d' % random.randrange(10**6)
r = s.post(B + '/account/authenticate', data={'username': USER, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''})
check('an ordinary user can register', r.status_code == 200 and USER in s.get(B + '/account').text, (r.status_code, r.url))
TITLE = '吾輩は猫である%d' % random.randrange(10**6)

home = s.get(B + '/').text
check('the front page offers "Add a book", with no AniList field', 'Add a book' in home and 'anilist-url' not in home)

# An ID that is not an Audible ASIN is not checked: the entry is made, unverified.
BOOK_ID = 'audiobook.jp %d' % random.randrange(10**6)
r = s.post(B + '/entry/create', data={'name': '  ' + TITLE.replace('は', 'は　') + ' ', 'book_id': BOOK_ID, 'anime': 'true'})
m = re.search(r'/entry/(\d+)', r.url)
check('a user (not an editor) makes a book entry without an ASIN', bool(m), (r.url, flashes(r.text)))
entry = int(m.group(1)) if m else sys.exit(1)
r = s.post(B + '/entry/create', data={'name': 'another title', 'book_id': BOOK_ID, 'anime': 'true'})
check('the same audiobook ID again is refused, and the entry is named', any('here already' in f and f'/entry/{entry}' in f for f in flashes(r.text)), flashes(r.text))
page = s.get(B + f'/entry/{entry}').text
check('the entry has the clean title, and is marked unverified', TITLE.replace('は', 'は ') in page and 'nverified' in page)

r = s.post(B + '/entry/create', data={'name': 'Ｗａｇａｈａｉ', 'book_id': '../../etc', 'anime': 'true'})
check('a bad audiobook ID is refused', any('audiobook ID' in f for f in flashes(r.text)), flashes(r.text))
r = s.post(B + '/entry/create', data={'name': TITLE + '！', 'anime': 'true'})
check('the same book typed another way is found, not made twice', any('here already' in f and f'/entry/{entry}' in f for f in flashes(r.text)), flashes(r.text))

import time
def patient(send):
    """The site limits how fast one visitor may post. A person is never this fast; wait as it asks."""
    for _ in range(8):
        r = send()
        if r.status_code != 429 and 'rate limit' not in r.text.lower(): return r
        time.sleep(8)
    return r
def upload(files):
    return patient(lambda: s.post(B + f'/entry/{entry}/upload', files=[('file', f) for f in files], headers={'Referer': B + f'/entry/{entry}'}))

r = upload([('neko.srt', book(400, '吾輩は猫である。名前はまだ無い。'), 'application/x-subrip')])
check('good subtitles are accepted', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))

r = upload([('page.srt', b'<!doctype html><html><body>404 not found</body></html>', 'text/html')])
check('a web page named .srt is refused, with the reason', any('No subtitle lines' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('episode.srt', book(24, 'こんにちは、世界。'), 'application/x-subrip')])
check('an episode is refused, with the reason', any('24 lines' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('english.srt', book(400, 'It was a dark and stormy night.'), 'application/x-subrip')])
check('English subtitles are refused on the Japanese site', any('not Japanese' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('sjis.srt', book(400, '吾輩は猫である。').decode('utf-8').encode('shift_jis'), 'application/x-subrip')])
check('Shift_JIS is refused, and the message says what to do', any('Save it as UTF-8' in f for f in flashes(r.text)), flashes(r.text))

def zipped(members):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', zipfile.ZIP_DEFLATED) as z:
        for n, d in members: z.writestr(n, d)
    return buf.getvalue()
r = upload([('bad.zip', zipped([('a.srt', book(400, '吾輩は猫である。')), ('setup.exe', b'MZ' + b'\0' * 100)]), 'application/zip')])
check('a zip with a program in it is refused', any('not a subtitle file' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('good.zip', zipped([('vol1/a.srt', book(400, '吾輩は猫である。')), ('vol1/b.srt', book(300, '名前はまだ無い。'))]), 'application/zip')])
check('a zip of good subtitles is accepted', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))
r = upload([('ok2.srt', book(400, 'どこで生れたか頓と見当がつかぬ。'), 'application/x-subrip'), ('junk.srt', b'junk', 'application/x-subrip')])
check('of two files, the good one goes in and the bad one is named', any('Uploaded 1 file' in f and 'junk.srt' in f for f in flashes(r.text)), flashes(r.text))

files = s.get(B + f'/api/entries/{entry}/files')
# --- the API, as subread.space uses it
r = patient(lambda: s.post(B + '/account/api_key', json={'new': True}, headers={'Referer': B + '/account'}))
key = None
try: key = r.json().get('token')
except Exception: pass
if not key:
    acct = s.get(B + '/account').text
    m = re.search(r'data-api-key="([^"]+)"|id="api-key"[^>]*value="([^"]+)"', acct)
    key = m and (m.group(1) or m.group(2))
check('the user gets an API key', bool(key), (r.status_code, r.text[:120]))
if key:
    api = requests.Session(); api.headers.update({'Authorization': key, 'Origin': 'https://subread.space'})
    # 草枕 (夏目漱石) on Audible; not on honjimaku.com on 2026-09-21
    r = patient(lambda: api.post(B + '/api/entries', json={'name': 'kusamakura ' + TITLE[-6:], 'book_id': 'B01J50DT5S'}))
    check('API: a user names a new book by its ASIN', r.status_code == 200 and 'entry_id' in r.json(), r.text[:200])
    first = r.json().get('entry_id')
    r = patient(lambda: api.post(B + '/api/entries', json={'name': 'whatever ' + TITLE[-6:], 'book_id': 'B01J50DT5S'}))
    check('API: the same ASIN again gives the same entry', r.status_code == 200 and r.json().get('entry_id') == first, r.text[:200])
    r = patient(lambda: api.post(B + '/api/entries', json={'name': '草　枕'}))
    check('API: the Audible title typed another way gives the same entry', r.status_code == 200 and r.json().get('entry_id') == first, r.text[:200])
    got = api.get(B + f'/api/entries/{first}').json()
    check('API: the entry carries book_id and the Audible title, and is verified', got.get('book_id') == 'B01J50DT5S' and got.get('name') == '草枕' and not got.get('flags', {}).get('unverified', True), got)
    r = patient(lambda: api.post(B + '/api/entries', json={'name': 'nothing', 'book_id': 'B0000000XX'}))
    check('API: an ASIN that Audible does not know is refused', r.status_code >= 400 and 'does not know' in r.text, r.text[:200])
    r = patient(lambda: api.post(B + f'/api/entries/{first}/upload', files=[('file', ('kusamakura%d.srt' % random.randrange(10**6), book(500, '山路を登りながら、こう考えた。'), 'application/x-subrip'))]))
    check('API: upload, and the answer carries no problems', r.status_code == 200 and r.json().get('problems') == [], r.text[:200])
    r = patient(lambda: api.post(B + f'/api/entries/{first}/upload', files=[('file', ('bad.srt', b'nothing', 'application/x-subrip'))]))
    check('API: a bad file is refused with the reason', r.status_code >= 400 and 'No subtitle lines' in r.text, r.text[:200])
    r = api.options(B + '/api/entries', headers={'Origin': 'https://subread.space', 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'authorization,content-type'})
    allowed = r.headers.get('access-control-allow-headers', '').lower()
    check('API: a page on subread.space may send JSON with its key (CORS preflight)', 'content-type' in allowed and 'authorization' in allowed, dict(r.headers))

# --- the verifier: Audible. A real ASIN names the entry; a false one is refused.
ASIN = 'B0C9BGK57W'  # 坊っちゃん, 夏目漱石, Japanese; not on honjimaku.com on 2026-09-21
AUDIBLE_TITLE = '坊っちゃん'
AUTHOR = '夏目 漱石'
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'rebuild world (typed by the user)', 'book_id': 'https://www.audible.co.jp/pd/x/' + ASIN + '?ref=x', 'anime': 'true'}))
m = re.search(r'/entry/(\d+)', r.url)
if not m:  # a second run: the site has it already, and says where
    m = re.search(r'/entry/(\d+)', ' '.join(flashes(r.text)))
check('a book with an Audible URL is made (or found again)', bool(m), (r.url, flashes(r.text)))
verified = int(m.group(1)) if m else None
page = s.get(B + f'/entry/{verified}').text if verified else ''
check('the entry takes the title as Audible writes it', AUDIBLE_TITLE in page, page[:0])
check('the entry is verified, not "unverified"', 'nverified' not in page and 'Audible ' + ASIN in page)
check('the note names the author from Audible', AUTHOR in page)

r = patient(lambda: s.post(B + '/entry/create', data={'name': 'another name', 'book_id': ASIN, 'anime': 'true'}))
check('the same ASIN again is refused, and the entry is named', any('here already' in f and f'/entry/{verified}' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'nothing', 'book_id': 'B0000000XX', 'anime': 'true'}))
check('an ASIN that Audible does not know is refused', any('does not know' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'Pride and Prejudice', 'book_id': 'B01BNU3A7K', 'anime': 'true'}))
check('an English audiobook is refused on the Japanese site', any('english' in f and 'japanese' in f for f in flashes(r.text)), flashes(r.text))

# the database: the ASIN is a column of its own, the entry is verified, and the file lands in the folder
if verified:
    entry = verified
    SRT = 'botchan%d.srt' % random.randrange(10**6)
    r = upload([(SRT, book(400, '親譲りの無鉄砲で小供の時から損ばかりしている。'), 'application/x-subrip')])
    check('an .srt uploads to the verified entry', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))
    import os, sqlite3
    db = os.environ.get('JIMAKU_DB')
    if db:
        con = sqlite3.connect(db)
        row = con.execute('SELECT name, book_id, flags, path FROM directory_entry WHERE id = ?', (verified,)).fetchone()
        check('DB: the row holds the ASIN in book_id and is not flagged unverified', row and row[1] == ASIN and (row[2] & 2) == 0, row)
        check('DB: the folder is named after the Audible title and the ASIN', row and row[3].endswith(f'{AUDIBLE_TITLE} [{ASIN}]') and os.path.isfile(os.path.join(row[3], SRT)), row and row[3])
        check('DB: book_id is unique', con.execute("SELECT count(*) FROM sqlite_master WHERE type='index' AND name='directory_entry_book_id_idx'").fetchone()[0] == 1)
        check('DB: older folders got their ASIN from the path', con.execute("SELECT count(*) FROM directory_entry WHERE path LIKE '%[B0________]' AND book_id IS NULL AND substr(path,-11,10) IN (SELECT substr(path,-11,10) FROM directory_entry GROUP BY 1 HAVING count(*)=1)").fetchone()[0] == 0)

# the site speaks of books only
home = s.get(B + '/').text
check('no "Live Action" tab, no "Jimaku"', 'Live Action' not in home and 'Jimaku' not in home)
r = s.get(B + '/dramas', allow_redirects=False)
check('/dramas goes to the front page', r.status_code in (301, 308) and r.headers.get('location') == '/')
check('the manifest is named after the site', 'Jimaku' not in s.get(B + '/site.webmanifest').text)
check('the API docs are named after the site', 'Jimaku' not in s.get(B + '/api/docs').text and 'Jimaku' not in s.get(B + '/api/openapi.json').text)

print(f'{sum(results)} of {len(results)} pass')
sys.exit(0 if all(results) else 1)
