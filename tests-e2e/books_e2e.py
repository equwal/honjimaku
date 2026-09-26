"""End-to-end check of the book features against a local test server (port 8433)."""
import io, json, os, re, sys, zipfile, urllib.parse
import requests

B = os.environ.get('JIMAKU_URL', 'http://localhost:8433')
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
dialog = re.search(r'<dialog id="upload-modal">(.*?)</dialog>', page, re.S)
check('the Upload button opens a dialog that names the book, audiobook and video files',
      '<button type="button" id="upload-button"' in page and bool(dialog)
      and all(ext in dialog.group(1) for ext in ('.srt', '.epub', '.pdf', '.m4b', '.opus', '.mp4', '.mkv'))
      and 'for="upload-file-input"' in dialog.group(1))

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

# --- books (.epub, .pdf), audiobooks (.m4b, .opus) and videos (.mp4, .mkv): each passes its
# own check. They go in alone, or with subtitles, so that a person can review the subtitles.
import pathlib, struct
FIX = pathlib.Path(__file__).resolve().parent.parent / 'tests' / 'fixtures'
NEKO = '吾輩は猫である。名前はまだ無い。'
def epub(text):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w') as z:
        z.writestr(zipfile.ZipInfo('mimetype'), 'application/epub+zip')
        z.writestr('META-INF/container.xml', '<container/>')
        z.writestr('OEBPS/p1.xhtml', '<html><head><title>x</title></head><body><p>%s</p></body></html>' % text)
    return buf.getvalue()
def padded_m4b(mb):
    """A real 10-minute M4B, made larger with an MP4 'free' box that players skip."""
    n = mb * 1024 * 1024
    return (FIX / 'silence-10m.m4b').read_bytes() + struct.pack('>I', 8 + n) + b'free' + b'\0' * n
M4B = (FIX / 'silence-10m.m4b').read_bytes()
OPUS = (FIX / 'silence-10m.opus').read_bytes()
MP4 = (FIX / 'video-10m.mp4').read_bytes()
MKV = (FIX / 'video-10m.mkv').read_bytes()
PDF = b'%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n'
def upload_to(eid, files):
    return patient(lambda: s.post(B + f'/entry/{eid}/upload', files=[('file', f) for f in files], headers={'Referer': B + f'/entry/{eid}'}))
def said(r, words):
    return any(words in f for f in flashes(r.text))
tag = random.randrange(10**6)

r = patient(lambda: s.post(B + '/entry/create', data={'name': 'bare book %d' % tag, 'book_id': 'audiobook.jp bare %d' % tag, 'anime': 'true'}))
m = re.search(r'/entry/(\d+)', r.url)
bare = int(m.group(1)) if m else sys.exit(1)
r = upload_to(bare, [('neko%d.epub' % tag, epub(NEKO * 50), 'application/epub+zip')])
check('an epub goes alone into an entry that has no subtitles', said(r, 'successful'), flashes(r.text))
r = upload_to(bare, [('neko%d.m4b' % tag, M4B, 'audio/mp4')])
check('an m4b goes alone into an entry that has no subtitles', said(r, 'successful'), flashes(r.text))
r = upload_to(bare, [('neko%d.mp4' % tag, MP4, 'video/mp4')])
check('an mp4 video goes in alone', said(r, 'successful'), flashes(r.text))
r = upload_to(bare, [('neko%d.mkv' % tag, MKV, 'video/x-matroska')])
check('an mkv video goes in alone', said(r, 'successful'), flashes(r.text))
r = upload_to(bare, [('neko%d.pdf' % tag, PDF, 'application/pdf')])
check('a pdf goes in alone', said(r, 'successful'), flashes(r.text))
r = upload_to(bare, [('pair%d.epub' % tag, epub(NEKO * 50), 'application/epub+zip'),
                     ('pair%d.m4b' % tag, M4B, 'audio/mp4'),
                     ('pair%d.srt' % tag, book(400, NEKO), 'application/x-subrip')])
page = s.get(B + f'/entry/{bare}').text
check('subtitles, an epub and an m4b in one upload are all accepted', said(r, 'successful') and all(('%s%d.%s' % (p, tag, x)) in page for p, x in [('neko', 'epub'), ('neko', 'm4b'), ('neko', 'mp4'), ('neko', 'mkv'), ('neko', 'pdf'), ('pair', 'srt')]), flashes(r.text))
r = s.get(B + f'/entry/{bare}/download/neko{tag}.mkv', stream=True)
check('a video downloads whole and is not compressed on the way', r.status_code == 200 and int(r.headers.get('content-length', 0)) == len(MKV) and 'content-encoding' not in r.headers, dict(r.headers))
r.close()

r = upload_to(entry, [('neko%d.opus' % tag, OPUS, 'audio/ogg')])
check('an opus audiobook goes into an entry that has good subtitles', said(r, 'successful'), flashes(r.text))
r = upload_to(entry, [('english%d.epub' % tag, epub('It was a dark and stormy night. ' * 50), 'application/epub+zip')])
check('an English epub is refused on the Japanese site', said(r, 'not Japanese'), flashes(r.text))
r = upload_to(entry, [('short%d.opus' % tag, (FIX / 'silence-1s.opus').read_bytes(), 'audio/ogg')])
check('a one-second opus is refused as not the whole audiobook', said(r, 'minutes long'), flashes(r.text))
r = upload_to(entry, [('fake%d.m4b' % tag, book(400, NEKO), 'audio/mp4')])
check('subtitles named .m4b are refused', said(r, 'not an M4B'), flashes(r.text))
r = upload_to(entry, [('fake%d.epub' % tag, b'PK\x03\x04 not really', 'application/epub+zip')])
check('a broken zip named .epub is refused', said(r, 'not an EPUB'), flashes(r.text))
r = upload_to(entry, [('fake%d.pdf' % tag, book(400, NEKO), 'application/pdf')])
check('subtitles named .pdf are refused', said(r, 'not a PDF'), flashes(r.text))
r = upload_to(entry, [('fake%d.mp4' % tag, book(400, NEKO), 'video/mp4')])
check('subtitles named .mp4 are refused', said(r, 'not an MP4'), flashes(r.text))
r = upload_to(entry, [('short%d.mkv' % tag, (FIX / 'video-1s.mkv').read_bytes(), 'video/x-matroska')])
check('a one-second mkv is refused as not the whole recording', said(r, 'minutes long'), flashes(r.text))
r = upload_to(entry, [('mute%d.mp4' % tag, (FIX / 'video-mute-10m.mp4').read_bytes(), 'video/mp4')])
check('a video without audio is refused', said(r, 'audio'), flashes(r.text))

big = 'big%d.m4b' % tag
r = upload_to(entry, [(big, padded_m4b(65), 'audio/mp4')])
check('a 65 MB audiobook passes the 16 MB limit of the other routes', said(r, 'successful'), (r.status_code, flashes(r.text)))
r = s.get(B + f'/entry/{entry}/download/{big}', stream=True)
check('the large audiobook downloads whole', r.status_code == 200 and int(r.headers.get('content-length', 0)) == len(M4B) + 8 + 65 * 1024 * 1024, dict(r.headers))
r.close()
r = patient(lambda: s.post(B + f'/entry/{entry}/bulk', json={'files': [big]}))
check('a bulk zip of more than 64 MB is refused, with the reason', r.status_code >= 400 and 'one by one' in r.text, (r.status_code, r.text[:200]))
try:
    # The server can answer 413 and close the connection before it reads the whole body.
    status = s.post(B + f'/entry/{entry}/report', data=b'x' * (17 * 1024 * 1024), headers={'Content-Type': 'application/json'}).status_code
except requests.exceptions.ConnectionError:
    status = 'closed'
check('other routes still refuse a body over 16 MB', status in (413, 'closed'), status)

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
check('an English audiobook is refused on the Japanese tab', any('english' in f.lower() and 'japanese' in f.lower() for f in flashes(r.text)), flashes(r.text))

# --- a tab for each language. Audible has one catalog for each country; the site asks each.
def made_or_found(r):
    m = re.search(r'/entry/(\d+)', r.url) or re.search(r'/entry/(\d+)', ' '.join(flashes(r.text)))
    return int(m.group(1)) if m else None
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'x', 'book_id': 'B01BNU3A7K', 'language': 'en', 'anime': 'true'}))
pride = made_or_found(r)
check('an English audiobook (Audible Japan) is made on the English tab', bool(pride), (r.url, flashes(r.text)))
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'x', 'book_id': 'B002V1OF70', 'language': 'en', 'anime': 'true'}))
dune = made_or_found(r)
check('an audiobook that only Audible US has is made', bool(dune) and 'Dune' in s.get(B + f'/entry/{dune}').text, (r.url, flashes(r.text)))
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'x', 'book_id': 'B002V1OF70', 'language': 'zz', 'anime': 'true'}))
check('a language code that is not ISO 639-1 is refused', any('ISO 639-1' in f for f in flashes(r.text)), flashes(r.text))
english = s.get(B + '/?lang=en').text
home = s.get(B + '/').text
check('the English tab lists the English books, the front page does not',
      f'/entry/{pride}"' in english and f'/entry/{dune}"' in english and f'/entry/{pride}"' not in home and f'/entry/{verified}"' in home)
check('the front page has a Japanese and an English tab', '>Japanese</a>' in home and 'href="/?lang=en">English</a>' in home)
check('the drop-down offers each ISO 639-1 language', home.count('<option value=') > 180 and '<option value="sw"' in english)
german = s.get(B + '/?lang=DE').text
check('a language with no books has a tab and a form of its own', 'Add a book in German' in german and 'name="language" value="de"' in german)
r = s.get(B + '/?lang=zz', allow_redirects=False)
check('an unknown language code goes to the front page', r.status_code in (302, 303) and r.headers.get('location') == '/')
if pride:
    entry = pride
    r = upload([('pride%d.srt' % random.randrange(10**6), book(400, 'It is a truth universally acknowledged.'), 'application/x-subrip')])
    check('English subtitles go into an English book', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))

# --- an RSS feed for each tab (each language, and each kind in it), beside the tab
import email.utils
import xml.etree.ElementTree as ET
def feed(path):
    r = s.get(B + path)
    return r, (ET.fromstring(r.content) if r.status_code == 200 else None)
def links(root):
    return [i.findtext('link') for i in root.iter('item')] if root is not None else []
r, en = feed('/feed.xml?lang=EN')
check('the English feed is RSS', en is not None and en.tag == 'rss' and r.headers.get('content-type', '').startswith('application/rss+xml'), (r.status_code, r.headers.get('content-type')))
check('the English feed names its language, its tab and itself',
      en is not None and en.findtext('channel/title', '').endswith('Books in English') and en.findtext('channel/language') == 'en'
      and en.findtext('channel/link') == B + '/?lang=en'
      and [l.get('href') for l in en.iter('{http://www.w3.org/2005/Atom}link')] == [B + '/feed.xml?lang=en'])
dates = [email.utils.parsedate_to_datetime(i.findtext('pubDate')) for i in en.iter('item')] if en is not None else []
check('the English feed has the English books, the newest first',
      bool(pride) and B + f'/entry/{pride}' in links(en) and B + f'/entry/{dune}' in links(en) and dates == sorted(dates, reverse=True), links(en)[:3])
r, ja = feed('/feed.xml')
check('the feed of the front page has the Japanese books, not the English ones', B + f'/entry/{bare}' in links(ja) and B + f'/entry/{pride}' not in links(ja), links(ja)[:3])
check('an unknown language has no feed', s.get(B + '/feed.xml?lang=zz').status_code == 404)
check('each tab names its feed for feed readers',
      '<link rel="alternate" type="application/rss+xml" href="/feed.xml?lang=en"' in s.get(B + '/?lang=en').text
      and '<link rel="alternate" type="application/rss+xml" href="/feed.xml"' in s.get(B + '/').text)
import html
r, en_anime = feed('/feed.xml?lang=en&kind=anime')
alternate = re.search(r'<link rel="alternate" type="application/rss\+xml" href="([^"]*)"', s.get(B + '/?lang=en&kind=anime').text)
check('the feed of a kind in a language links to its tab and to itself',
      en_anime is not None and en_anime.findtext('channel/title', '').endswith('Anime in English')
      and en_anime.findtext('channel/link') == B + '/?lang=en&kind=anime'
      and [l.get('href') for l in en_anime.iter('{http://www.w3.org/2005/Atom}link')] == [B + '/feed.xml?lang=en&kind=anime']
      and alternate is not None and html.unescape(alternate.group(1)) == '/feed.xml?lang=en&kind=anime', (r.status_code, alternate and alternate.group(1)))
check('an unknown kind has no feed', s.get(B + '/feed.xml?kind=manga').status_code == 404)
r = s.get(B + '/dramas/feed.xml', allow_redirects=False)
check('/dramas/feed.xml goes to the feed of the live action shows', r.status_code in (301, 308) and r.headers.get('location') == '/feed.xml?kind=drama', (r.status_code, r.headers.get('location')))

# --- the blue check mark: an editor says that a person has reviewed the subtitles
import os, sqlite3
db = os.environ.get('JIMAKU_DB')
if db:
    ed = requests.Session(); ed.headers['Referer'] = B + '/'
    EDITOR = 'reviewer%d' % random.randrange(10**6)
    # Registration answers with a redirect. Following it would make the first request with the session,
    # and the server caches the account at that request. So the flag goes into the database first.
    r = ed.post(B + '/account/authenticate', data={'username': EDITOR, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''}, allow_redirects=False)
    con = sqlite3.connect(db); con.execute('UPDATE account SET flags = 2 WHERE name = ?', (EDITOR,)); con.commit(); con.close()
    page = ed.get(B + f'/entry/{bare}').text
    check('the editor sees a Reviewed box in the edit dialog, not ticked', 'id="entry-reviewed"' in page and 'value="true"checked' not in page.split('name="reviewed"')[0][-80:] and 'class="title"' in page, page.count('entry-reviewed'))
    def edit_entry(reviewed):
        data = {'name': 'bare book %d' % tag, 'japanese_name': '', 'english_name': '', 'book_id': 'audiobook.jp bare %d' % tag, 'notes': '', 'anime': 'true'}
        if reviewed: data['reviewed'] = 'true'
        return ed.post(B + f'/entry/{bare}/edit', data=data, headers={'Referer': B + f'/entry/{bare}'})
    r = edit_entry(True)
    page = s.get(B + f'/entry/{bare}').text
    home = s.get(B + '/').text
    check('the editor ticks Reviewed', said(r, 'edited'), (r.status_code, flashes(r.text)))
    check('the entry page shows the blue check mark beside the title', 'class="title reviewed"' in page and 'reviewed.svg' in s.get(B + '/static/entry.css').text)
    check('the front page shows the blue check mark beside the name', f'href="/entry/{bare}" class="table-data file-name reviewed"' in home)
    check('the check mark image is served', s.get(B + '/static/reviewed.svg').status_code == 200)
    if key:
        got = api.get(B + f'/api/entries/{bare}').json()
        check('API: the entry carries reviewed: true', got.get('flags', {}).get('reviewed') is True, got.get('flags'))
    r = edit_entry(False)
    page = s.get(B + f'/entry/{bare}').text
    check('the editor unticks Reviewed, and the mark goes', said(r, 'edited') and 'class="title"' in page and 'title reviewed' not in page, flashes(r.text))
    r = s.post(B + f'/entry/{bare}/edit', data={'name': 'x', 'japanese_name': '', 'english_name': '', 'notes': '', 'anime': 'true', 'reviewed': 'true'}, headers={'Referer': B + f'/entry/{bare}'})
    check('a user who is not an editor cannot mark an entry as reviewed', said(r, 'permissions') and 'title reviewed' not in s.get(B + f'/entry/{bare}').text, flashes(r.text))

    # --- other names: the romaji and the title of the English edition. The search finds the book by each.
    ROMAJI, ENGLISH, OTHER = 'Hadaka no Hon %d' % tag, 'The Bare Book, Vol. %d' % tag, '裸の本 %d' % tag
    r = ed.post(B + f'/entry/{bare}/edit', data={'name': 'bare book %d' % tag, 'japanese_name': '', 'english_name': ENGLISH, 'other_names': f'{ROMAJI}\r\n\r\n {OTHER} \r\n{ROMAJI}',
                                                'book_id': 'audiobook.jp bare %d' % tag, 'notes': '', 'anime': 'true'}, headers={'Referer': B + f'/entry/{bare}'})
    page = s.get(B + f'/entry/{bare}').text
    check('the editor gives the romaji, another title and the English name', said(r, 'edited') and f'{ENGLISH} · {ROMAJI} · {OTHER}' in page, flashes(r.text))
    check('the edit dialog shows the other names one on each line', f'{ROMAJI}\n{OTHER}</textarea>' in ed.get(B + f'/entry/{bare}').text)
    check('the front page carries the other names for the search', f'{ROMAJI}' in s.get(B + '/').text)
    if key:
        found = [e['id'] for e in api.get(B + '/api/entries/search', params={'query': ROMAJI.lower()}).json()]
        check('API: the search finds the book by its romaji', bare in found, found[:5])
        found = [e['id'] for e in api.get(B + '/api/entries/search', params={'query': 'bare book, vol. %d' % tag}).json()]
        check('API: the search finds the book by its English name', bare in found, found[:5])
        got = api.get(B + f'/api/entries/{bare}').json()
        check('API: the entry carries its other names, each once', got.get('other_names') == [ROMAJI, OTHER], got.get('other_names'))
    r = edit_entry(False)
    check('an edit without the field keeps the other names', said(r, 'edited') and ROMAJI in s.get(B + f'/entry/{bare}').text, flashes(r.text))
    logs = ed.get(B + '/audit-logs', params={'entry_id': bare}).text
    check('the audit log has the change of the other names', 'other_names' in logs and ROMAJI in logs, logs[:200])

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
        check('DB: a book made on a tab holds the code of its language', not pride or con.execute('SELECT language FROM directory_entry WHERE id = ?', (pride,)).fetchone()[0] == 'en')
        check('DB: book_id is unique', con.execute("SELECT count(*) FROM sqlite_master WHERE type='index' AND name='directory_entry_book_id_idx'").fetchone()[0] == 1)
        check('DB: older folders got their ASIN from the path', con.execute("SELECT count(*) FROM directory_entry WHERE path LIKE '%[B0________]' AND book_id IS NULL AND substr(path,-11,10) IN (SELECT substr(path,-11,10) FROM directory_entry GROUP BY 1 HAVING count(*)=1)").fetchone()[0] == 0)

# the site has a Live Action tab next to the books, and does not say Jimaku
home = s.get(B + '/').text
check('a "Live Action" tab, no "Jimaku"', re.search(r'<a [^>]*href="[^"]*kind=drama"[^>]*>Live Action</a>', home) is not None and 'Jimaku' not in home)
r = s.get(B + '/dramas', allow_redirects=False)
check('/dramas goes to the Live Action tab', r.status_code in (301, 308) and r.headers.get('location') == '/?kind=drama', (r.status_code, r.headers.get('location')))
check('the manifest is named after the site', 'Jimaku' not in s.get(B + '/site.webmanifest').text)
check('the API docs are named after the site', 'Jimaku' not in s.get(B + '/api/docs').text and 'Jimaku' not in s.get(B + '/api/openapi.json').text)

print(f'{sum(results)} of {len(results)} pass')
sys.exit(0 if all(results) else 1)
