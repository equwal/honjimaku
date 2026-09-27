"""End-to-end check of the release check against a local test server.

The script serves a fake release search (an RSS feed) on port 8434. Start the site with
this in its config, as a site for books:

    "release_index": {"url": "http://127.0.0.1:8434/rss?q={query}"}

Then run the script. JIMAKU_URL gives the address of the site (http://localhost:8435).
"""
import datetime, email.utils, html, http.server, os, random, re, sys, threading, time, urllib.parse
import requests

B = os.environ.get('JIMAKU_URL', 'http://localhost:8435')
FEED_PORT = 8434

NOW = datetime.datetime.now(datetime.timezone.utc)
def days_ago(n):
    return email.utils.format_datetime(NOW - datetime.timedelta(days=n))

# The releases that the fake search knows, by show. A search finds a show when it has all
# the words of the name of the show.
SHOWS = {
    'fake show': [
        ('Fake.Show.S01E01.Pilot.1080p.WEB-DL.x264-GRP', days_ago(600)),
        ('Fake.Show.S01E02.Second.Night.1080p.WEB-DL.x264-GRP', days_ago(593)),
        ('Fake.Show.S01E03.1080p.WEB-DL.x264-GRP', days_ago(586)),
        # Another show with a longer name: it is not "Fake Show".
        ('Fake.Show.Returns.S01E09.1080p.WEB-DL.x264-GRP', days_ago(100)),
    ],
    'lonely show': [('Lonely.Show.S01E01.1080p.WEB-DL.x264-GRP', days_ago(600))],
    'fresh show': [
        ('Fresh.Show.S01E01.1080p.WEB-DL.x264-GRP', days_ago(2)),
        ('Fresh.Show.S01E02.1080p.WEB-DL.x264-GRP', days_ago(1)),
    ],
}
queries = []

class Feed(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query).get('q', [''])[0]
        queries.append(query)
        words = set(query.lower().split())
        items = [item for show, found in SHOWS.items() if set(show.split()) <= words for item in found]
        body = ''.join(
            '<item><title>%s</title><pubDate>%s</pubDate><nyaa:seeders>1</nyaa:seeders></item>' % (html.escape(name), date)
            for name, date in items
        )
        xml = ('<?xml version="1.0" encoding="utf-8"?><rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">'
               '<channel><title>fake</title>%s</channel></rss>' % body).encode('utf-8')
        self.send_response(200)
        self.send_header('Content-Type', 'application/rss+xml')
        self.send_header('Content-Length', str(len(xml)))
        self.end_headers()
        self.wfile.write(xml)

    def log_message(self, *args):
        pass

server = http.server.ThreadingHTTPServer(('127.0.0.1', FEED_PORT), Feed)
threading.Thread(target=server.serve_forever, daemon=True).start()

results = []
def check(name, ok, detail=''):
    results.append(ok)
    print(('PASS ' if ok else 'FAIL ') + name + (' | ' + str(detail)[:300] if detail else ''))

def flashes(page):
    return [html.unescape(re.sub(r'<[^>]+>', ' ', m)).strip() for m in re.findall(r'class="alert[^"]*"[^>]*>\s*<p>(.*?)</p>', page, re.S)]

def patient(send):
    """The site limits how fast one visitor may post. Wait as it asks."""
    for _ in range(8):
        r = send()
        if r.status_code != 429 and 'rate limit' not in r.text.lower():
            return r
        time.sleep(8)
    return r

def stamp(t):
    ms = int(round(t * 1000))
    return '%02d:%02d:%02d,%03d' % (ms // 3600000, ms // 60000 % 60, ms // 1000 % 60, ms % 1000)

def episode(n, line):
    return '\n'.join('%d\n%s --> %s\n%s\n' % (i + 1, stamp(i * 4), stamp(i * 4 + 3.5), line) for i in range(n)).encode('utf-8')

s = requests.Session()
s.headers['Referer'] = B + '/'
USER = 'viewer%d' % random.randrange(10**6)
r = s.post(B + '/account/authenticate', data={'username': USER, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''})
check('an ordinary user can register', r.status_code == 200 and USER in s.get(B + '/account').text, (r.status_code, r.url))

home = s.get(B + '/?kind=drama').text
tmdb_field = re.search(r'<input[^>]*name="tmdb_url"[^>]*>', home)
check('the Live Action form offers a name for a show without a TMDB page',
      'for a show without that page' in home and tmdb_field is not None and 'required' not in tmdb_field.group(0),
      tmdb_field and tmdb_field.group(0))

def create(name):
    return patient(lambda: s.post(B + '/entry/create', data={'name': name, 'kind': 'drama', 'language': 'ja', 'anime': 'false'}))

r = create('Fake Show')
m = re.search(r'/entry/(\d+)', r.url)
check('a show that three old releases name is added', bool(m), (r.url, flashes(r.text)))
entry = int(m.group(1)) if m else sys.exit(1)
page = s.get(B + f'/entry/{entry}').text
check('the new entry is unverified, and its note gives the evidence',
      'nverified' in page and 'Added from the release check: 3 releases name this show' in page, re.findall(r'release check[^<]*', page))
check('the search was asked for the name of the show', 'Fake Show' in queries, queries)

r = create('Fake Show: Part 2')
check('the same show with a part is the entry that is there already',
      any('here already' in f and f'/entry/{entry}' in f for f in flashes(r.text)), flashes(r.text))
r = create('Nothing Show %d' % random.randrange(10**6))
check('a show that no release names is refused', any('found no release' in f for f in flashes(r.text)), flashes(r.text))
r = create('Lonely Show')
check('a show with too few releases is refused', any('only 1 release' in f and '2 are necessary' in f for f in flashes(r.text)), flashes(r.text))
r = create('Fresh Show')
check('a show whose first release is too new is refused', any('must be 7 days old' in f for f in flashes(r.text)), flashes(r.text))

def upload(files):
    return patient(lambda: s.post(B + f'/entry/{entry}/upload', files=[('file', f) for f in files], headers={'Referer': B + f'/entry/{entry}'}))

LINE = 'テラスハウスへようこそ。今日から新しい生活が始まります。'
r = upload([('Fake.Show.S01E02.Second.Night.WEBRip.ja.srt', episode(400, LINE), 'application/x-subrip')])
check('a file for an episode that a release names is accepted with a note',
      any('successful' in f.lower() and 'Release check: 1 of 1 files match' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('Fake.Show.S01E09.WEBRip.ja.srt', episode(400, LINE), 'application/x-subrip')])
check('a file for an episode that no release names is accepted, and the note names it',
      any('Fake.Show.S01E09.WEBRip.ja.srt: no release of the show names this episode' in f for f in flashes(r.text)), flashes(r.text))
r = upload([('notes.srt', episode(400, LINE), 'application/x-subrip')])
check('a file with no episode in its name gets no release note',
      any('successful' in f.lower() and 'Release check' not in f for f in flashes(r.text)), flashes(r.text))

# The audit log is for editors. JIMAKU_DB is the database of the test site: an editor is
# made there, before the first request of the editor caches the account.
DB = os.environ.get('JIMAKU_DB')
if DB:
    import sqlite3
    editor = requests.Session()
    editor.headers['Referer'] = B + '/'
    NAME = 'editor%d' % random.randrange(10**6)
    editor.post(B + '/account/authenticate', allow_redirects=False,
                data={'username': NAME, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''})
    with sqlite3.connect(DB) as con:
        con.execute('UPDATE account SET flags = 2 WHERE name = ?', (NAME,))
    logs = editor.get(B + '/audit-logs', params={'entry_id': entry, 'type': 'upload'})
    unmatched = [log['data'].get('unmatched', []) for log in logs.json().get('logs', [])] if logs.ok else []
    check('the audit log names the file that no release names',
          ['Fake.Show.S01E09.WEBRip.ja.srt'] in unmatched, (logs.status_code, unmatched))
else:
    print('SKIP the audit log: set JIMAKU_DB to the database of the test site')

server.shutdown()
print('%d of %d checks passed' % (sum(results), len(results)))
sys.exit(0 if all(results) else 1)
