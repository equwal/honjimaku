"""End-to-end check of the three tabs of a site for books: Books, Anime and Live Action.

Run it against a test server of a site for books (book_site = true, subtitle_language = "ja").
JIMAKU_URL names the server (default http://localhost:8433). JIMAKU_DB names its database, for
the checks of the rows and to make an editor. AniList must answer. Set JIMAKU_TMDB=1 only if
the config of the server has a tmdb_api_key: then a live action show is made from TMDB too.
"""
import os, random, re, sqlite3, sys, time
import requests

B = os.environ.get('JIMAKU_URL', 'http://localhost:8433')
DB = os.environ.get('JIMAKU_DB')

def stamp(t):
    ms = int(round(t * 1000)); return '%02d:%02d:%02d,%03d' % (ms // 3600000, ms // 60000 % 60, ms // 1000 % 60, ms % 1000)
def episode(n, line):
    return '\n'.join('%d\n%s --> %s\n%s\n' % (i + 1, stamp(i * 4), stamp(i * 4 + 3.5), line) for i in range(n)).encode('utf-8')
def flashes(html):
    import html as H
    return [H.unescape(re.sub(r'<[^>]+>', ' ', m)).strip() for m in re.findall(r'class="alert[^"]*"[^>]*>\s*<p>(.*?)</p>', html, re.S)]
def patient(send):
    for _ in range(8):
        r = send()
        if r.status_code != 429 and 'rate limit' not in r.text.lower(): return r
        time.sleep(8)
    return r
def entry_of(r):
    m = re.search(r'/entry/(\d+)$', r.url) or re.search(r'/entry/(\d+)', ' '.join(flashes(r.text)))
    return int(m.group(1)) if m else None
def nav(html):
    return [(bool(a), href, name) for a, href, name in re.findall(r'<a class="nav-item ?(active)?" href="([^"]*)">([^<]*)</a>', html)]
def register(session):
    name = 'viewer%d' % random.randrange(10**6)
    r = session.post(B + '/account/authenticate', allow_redirects=False,
                     data={'username': name, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''})
    return name, r

results = []
def check(name, ok, detail=''):
    results.append(ok); print(('PASS ' if ok else 'FAIL ') + name + (' | ' + str(detail)[:230] if detail else ''))

s = requests.Session(); s.headers['Referer'] = B + '/'
USER, r = register(s)
check('an ordinary user can register', r.status_code in (302, 303) and USER in s.get(B + '/account').text, (r.status_code, r.headers.get('location')))
# The server keeps an account in a cache from its first request, so the editor gets its flag before that.
ed = requests.Session(); ed.headers['Referer'] = B + '/'
EDITOR = None
if DB:
    EDITOR, r = register(ed)
    con = sqlite3.connect(DB)
    con.execute('UPDATE account SET flags = 2 WHERE name = ?', (EDITOR,)); con.commit()

# --- the three tabs
books = s.get(B + '/').text
anime = s.get(B + '/?kind=anime').text
drama = s.get(B + '/?kind=drama').text
for page, active in ((books, 'Books'), (anime, 'Anime'), (drama, 'Live Action')):
    tabs = [t for t in nav(page) if t[2] in ('Books', 'Anime', 'Live Action')]
    check(f'the {active} page has the three tabs, and its own is active',
          [t[2] for t in tabs] == ['Books', 'Anime', 'Live Action'] and [t[2] for t in tabs if t[0]] == [active], tabs)
check('the Books tab keeps the Audible form', 'Add a book' in books and 'book-id' in books and 'anilist-url' not in books and 'tmdb-url' not in books)
check('the Anime tab asks for an AniList page, and says the kind and the language',
      'Add an anime in Japanese' in anime and 'anilist-url' in anime and 'tmdb-url' not in anime and 'book-id' not in anime
      and 'name="kind" value="anime"' in anime and 'name="language" value="ja"' in anime and 'upload-button' in anime)
check('the Live Action tab asks for a TMDB page, and says the kind and the language',
      'Add a live action show in Japanese' in drama and 'tmdb-url' in drama and 'anilist-url' not in drama and 'book-id' not in drama
      and 'name="kind" value="drama"' in drama and 'name="language" value="ja"' in drama and 'upload-button' in drama)
check('for a user, the AniList and TMDB fields are required',
      re.search(r'<input class="form-field"\s*required[^>]*id="anilist-url"', anime) is not None
      and re.search(r'<input class="form-field"\s*required[^>]*id="tmdb-url"', drama) is not None)
r = s.get(B + '/dramas', allow_redirects=False)
check('/dramas goes to the Live Action tab', r.status_code in (301, 308) and r.headers.get('location') == '/?kind=drama', (r.status_code, r.headers.get('location')))
help_page = s.get(B + '/help').text
menu = [(href, name) for _, href, name in nav(help_page) if name in ('Books', 'Anime', 'Live Action')]
check('the menu of every page has the three tabs', menu == [('/', 'Books'), ('/?kind=anime', 'Anime'), ('/?kind=drama', 'Live Action')], menu)
check('the help says how to add a show', 'AniList page of the anime' in help_page and 'TMDB page of the show' in help_page)

# --- the verifiers
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'no source', 'kind': 'anime', 'anime': 'true', 'language': 'ja'}))
check('a user cannot make an anime without an AniList page', any('AniList' in f for f in flashes(r.text)) and entry_of(r) is None, flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'name': 'no source', 'kind': 'drama', 'anime': 'false', 'language': 'ja'}))
check('a user cannot make a live action show without a TMDB page', any('TMDB' in f for f in flashes(r.text)) and entry_of(r) is None, flashes(r.text))

ANILIST = 154587  # 葬送のフリーレン (Frieren)
URL = f'https://anilist.co/anime/{ANILIST}/Sousou-no-Frieren/'
r = patient(lambda: s.post(B + '/entry/create', data={'anilist_url': URL, 'kind': 'anime', 'anime': 'true', 'language': 'ja'}))
show = entry_of(r)
check('an AniList URL makes (or finds) the anime', show is not None, (r.url, flashes(r.text)))
page = s.get(B + f'/entry/{show}').text if show else ''
check('the anime takes its names from AniList and is verified', 'Sousou no Frieren' in page and '葬送のフリーレン' in page and 'nverified' not in page)
r = patient(lambda: s.post(B + '/entry/create', data={'anilist_url': URL, 'kind': 'anime', 'anime': 'true', 'language': 'ja'}))
check('the same anime again is refused, and the entry is named', any('here already' in f and f'/entry/{show}' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'anilist_url': 'https://anilist.co/anime/999999999/', 'kind': 'anime', 'anime': 'true', 'language': 'ja'}))
check('an AniList ID that AniList does not know is refused', any('AniList' in f for f in flashes(r.text)) and entry_of(r) is None, flashes(r.text))

anime = s.get(B + '/?kind=anime').text
books = s.get(B + '/').text
check('the anime is listed on the Anime tab, not on the Books tab', f'href="/entry/{show}"' in anime and f'href="/entry/{show}"' not in books)

r = patient(lambda: s.post(B + f'/entry/{show}/upload', files=[('file', ('ep%d.srt' % random.randrange(10**6), episode(300, '魔王を倒した勇者一行の、その後の物語。'), 'application/x-subrip'))], headers={'Referer': B + f'/entry/{show}'}))
check('Japanese subtitles upload to the anime', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))

if os.environ.get('JIMAKU_TMDB'):
    r = patient(lambda: s.post(B + '/entry/create', data={'tmdb_url': 'https://www.themoviedb.org/tv/1399', 'kind': 'drama', 'anime': 'false', 'language': 'ja'}))
    tv = entry_of(r)
    check('a TMDB URL makes (or finds) the live action show', tv is not None, (r.url, flashes(r.text)))
    check('the show is listed on the Live Action tab', tv is not None and f'href="/entry/{tv}"' in s.get(B + '/?kind=drama').text)

# --- the API: an AniList ID makes an anime, and the same ID gives the same entry.
r = patient(lambda: s.post(B + '/account/api_key', json={'new': True}, headers={'Referer': B + '/account'}))
key = None
try: key = r.json().get('token')
except Exception: pass
check('the user gets an API key', bool(key), (r.status_code, r.text[:120]))
if key:
    api = requests.Session(); api.headers['Authorization'] = key
    r = patient(lambda: api.post(B + '/api/entries', json={'anilist_id': ANILIST}))
    check('API: the same AniList ID gives the same entry', r.status_code == 200 and r.json().get('entry_id') == show, r.text[:200])
    r = patient(lambda: api.post(B + '/api/entries', json={'anilist_id': ANILIST, 'name': 'A name of my own'}))
    check('API: a user cannot name a show', r.status_code == 403, (r.status_code, r.text[:200]))
    r = api.get(B + '/api/entries/search', params={'kind': 'anime', 'anilist_id': ANILIST})
    check('API: the search finds the anime by its kind', r.status_code == 200 and [e['id'] for e in r.json()] == [show], r.text[:200])

if DB and show:
    row = con.execute('SELECT kind, language, anilist_id, flags FROM directory_entry WHERE id = ?', (show,)).fetchone()
    check('DB: the anime row has kind "anime", language "ja", the AniList ID, the anime flag, and no unverified flag',
          row and row[0] == 'anime' and row[1] == 'ja' and row[2] == ANILIST and (row[3] & 1) == 1 and (row[3] & 2) == 0, row)
    # An editor edits the anime. The form must keep its AniList ID and its tab.
    page = ed.get(B + f'/entry/{show}').text
    check('the edit form of a show asks for AniList and TMDB, not for an audiobook',
          'entry-anilist-id' in page and 'entry-tmdb-url' in page and 'entry-book-id' not in page and f'value="{ANILIST}"' in page)
    form = {'name': 'Sousou no Frieren', 'japanese_name': '葬送のフリーレン', 'english_name': 'Frieren: Beyond Journey’s End',
            'anilist_id': str(ANILIST), 'tmdb_url': '', 'bangumi_id': '', 'notes': 'edited', 'anime': 'true'}
    r = ed.post(B + f'/entry/{show}/edit', data=form, headers={'Referer': B + f'/entry/{show}'})
    row = con.execute('SELECT kind, anilist_id, flags, notes FROM directory_entry WHERE id = ?', (show,)).fetchone()
    check('an edit keeps the AniList ID, the anime flag and the kind', row and row[0] == 'anime' and row[1] == ANILIST and (row[2] & 1) == 1 and row[3] == 'edited', (row, flashes(r.text)))
    NAMED = 'An editor names a show %d' % random.randrange(10**6)
    r = patient(lambda: ed.post(B + '/entry/create', data={'name': NAMED, 'kind': 'drama', 'anime': 'false', 'language': 'ja'}))
    named = entry_of(r)
    row = con.execute('SELECT kind, flags, path FROM directory_entry WHERE id = ?', (named,)).fetchone() if named else None
    check('an editor may name a live action show without TMDB, and it goes to the Live Action tab',
          row and row[0] == 'drama' and (row[1] & 1) == 0 and os.path.basename(row[2]).startswith('[drama] ')
          and f'href="/entry/{named}"' in s.get(B + '/?kind=drama').text, (flashes(r.text), row))

print(f'{sum(results)} of {len(results)} pass')
sys.exit(0 if all(results) else 1)
