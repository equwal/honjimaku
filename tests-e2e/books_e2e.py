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

r = s.post(B + '/entry/create', data={'name': '  ' + TITLE.replace('は', 'は　') + ' ', 'book_id': 'B0TEST1234', 'anime': 'true'})
m = re.search(r'/entry/(\d+)', r.url)
check('a user (not an editor) makes a book entry', bool(m), r.url)
entry = int(m.group(1))
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
    r = patient(lambda: api.post(B + '/api/entries', json={'name': '１Ｑ８４ BOOK' + TITLE[-6:], 'book_id': 'B0BPXSSWVF'}))
    check('API: a user names a new book', r.status_code == 200 and 'entry_id' in r.json(), r.text[:200])
    first = r.json().get('entry_id')
    r = patient(lambda: api.post(B + '/api/entries', json={'name': '1q84 book' + TITLE[-6:]}))
    check('API: the same book again gives the same entry', r.status_code == 200 and r.json().get('entry_id') == first, r.text[:200])
    r = patient(lambda: api.post(B + f'/api/entries/{first}/upload', files=[('file', ('1q84.ja.srt', book(500, '青豆はタクシーの中で音楽を聴いていた。'), 'application/x-subrip'))]))
    check('API: upload, and the answer carries no problems', r.status_code == 200 and r.json().get('problems') == [], r.text[:200])
    r = patient(lambda: api.post(B + f'/api/entries/{first}/upload', files=[('file', ('bad.srt', b'nothing', 'application/x-subrip'))]))
    check('API: a bad file is refused with the reason', r.status_code >= 400 and 'No subtitle lines' in r.text, r.text[:200])
    r = api.options(B + '/api/entries', headers={'Origin': 'https://subread.space', 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'authorization,content-type'})
    allowed = r.headers.get('access-control-allow-headers', '').lower()
    check('API: a page on subread.space may send JSON with its key (CORS preflight)', 'content-type' in allowed and 'authorization' in allowed, dict(r.headers))

print(f'{sum(results)} of {len(results)} pass')
sys.exit(0 if all(results) else 1)
