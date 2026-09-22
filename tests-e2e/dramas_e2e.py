"""End-to-end check of a site for Chinese shows (drama_site = true) against a local test server (port 8434).

The local database is a copy of dung.live, with the path of entry 1 pointed at a local folder.
JIMAKU_DB names that database for the checks of the rows.
"""
import os, random, re, sys, time
import requests

B = 'http://localhost:8434'
s = requests.Session()
s.headers['Referer'] = B + '/'

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

results = []
def check(name, ok, detail=''):
    results.append(ok); print(('PASS ' if ok else 'FAIL ') + name + (' | ' + str(detail)[:230] if detail else ''))

USER = 'viewer%d' % random.randrange(10**6)
r = s.post(B + '/account/authenticate', data={'username': USER, 'password': 'correct horse battery', 'action': 'register', 'session_description': ''})
check('an ordinary user can register', r.status_code == 200 and USER in s.get(B + '/account').text, (r.status_code, r.url))

home = s.get(B + '/').text
check('one "Dramas" tab, no "Anime", no "Live Action"', '>Dramas<' in home and '>Anime<' not in home and 'Live Action' not in home)
check('the search box speaks of shows and Bangumi', 'Search shows by name or Bangumi URL' in home)
check('the form asks for a Bangumi page, not an AniList or TMDB URL', 'bangumi-url' in home and 'anilist-url' not in home and 'tmdb-url' not in home)
check('the page says Chinese subtitles', 'Chinese subtitles' in home and 'Japanese subtitles' not in home)
check('every entry is listed, whatever its flag', home.count('class="entry"') >= 74, home.count('class="entry"'))
r = s.get(B + '/dramas?query=x', allow_redirects=False)
check('/dramas goes to the front page and keeps the query', r.status_code in (301, 308) and r.headers.get('location') == '/?query=x', (r.status_code, r.headers.get('location')))
check('the anime OpenSearch is gone, the drama one stays', s.get(B + '/opensearch/anime.xml').status_code == 404 and s.get(B + '/opensearch/dramas.xml').status_code == 200)
help_page = s.get(B + '/help').text
check('the help page names Bangumi, not AniList or TMDB', 'AniList Integration' not in help_page and 'AniList URL' not in help_page and 'Live Action' not in help_page and 'bgm.tv' in help_page)
check('the account page has no AniList form', 'AniList' not in s.get(B + '/account').text)

# --- the verifier: Bangumi. A real subject names the entry; a false one is refused.
r = s.post(B + '/entry/create', data={'name': 'no source', 'anime': 'false'})
check('a user cannot make an entry without a Bangumi page', any('Bangumi' in f for f in flashes(r.text)), flashes(r.text))
BGM = 258207  # 陈情令 (The Untamed), a live action drama, bgm.tv type 6
r = patient(lambda: s.post(B + '/entry/create', data={'bangumi_url': 'https://bgm.tv/subject/%d?tab=ep' % BGM, 'anime': 'false'}))
m = re.search(r'/entry/(\d+)', r.url) or re.search(r'/entry/(\d+)', ' '.join(flashes(r.text)))
check('a Bangumi URL makes (or finds) the entry', bool(m), (r.url, flashes(r.text)))
show = int(m.group(1)) if m else None
page = s.get(B + f'/entry/{show}').text if show else ''
check('the entry takes the name from Bangumi, is verified, and links to Bangumi', '陈情令' in page and 'nverified' not in page and 'bgm.tv/subject/258207' in page)
r = patient(lambda: s.post(B + '/entry/create', data={'bangumi_url': str(BGM), 'anime': 'false'}))
check('the same subject again is refused, and the entry is named', any('here already' in f and f'/entry/{show}' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'bangumi_url': '999999999', 'anime': 'false'}))
check('a subject that Bangumi does not know is refused', any('does not know' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'bangumi_url': 'https://bgm.tv/subject/5', 'anime': 'false'}))
check('a subject that is not a show (a book) is refused', any('not a show' in f for f in flashes(r.text)), flashes(r.text))
r = patient(lambda: s.post(B + '/entry/create', data={'bangumi_url': 'https://bgm.tv/subject/174222', 'anime': 'true'}))
m2 = re.search(r'/entry/(\d+)', r.url) or re.search(r'/entry/(\d+)', ' '.join(flashes(r.text)))
check('a donghua is made (or found) too, with its Chinese name', bool(m2) and '爱神巧克力 第二季' in s.get(B + f'/entry/{m2.group(1)}').text, (r.url, flashes(r.text)))

import sqlite3
db = os.environ.get('JIMAKU_DB')
if db and show:
    con = sqlite3.connect(db)
    row = con.execute('SELECT name, bangumi_id, flags, path, japanese_name FROM directory_entry WHERE id = ?', (show,)).fetchone()
    check('DB: the row holds the Bangumi number, is not flagged unverified, and is not flagged anime', row and row[1] == BGM and (row[2] & 2) == 0 and (row[2] & 1) == 0, row)
    check('DB: the folder is named after the show and the subject', row and row[3].endswith(f'陈情令 [bgm-{BGM}]') and os.path.isdir(row[3]), row and row[3])
    check('DB: bangumi_id is unique', con.execute("SELECT count(*) FROM sqlite_master WHERE type='index' AND name='directory_entry_bangumi_id_idx'").fetchone()[0] == 1)
    SRT2 = 'ep%d.srt' % random.randrange(10**6)
    entry = show
    r = patient(lambda: s.post(B + f'/entry/{entry}/upload', files=[('file', (SRT2, episode(300, '蓝忘机，你还记得吗？'), 'application/x-subrip'))], headers={'Referer': B + f'/entry/{entry}'}))
    check('an .srt uploads to the verified show', any('successful' in f.lower() for f in flashes(r.text)) and os.path.isfile(os.path.join(row[3], SRT2)), flashes(r.text))

entry = 1
def upload(files):
    return patient(lambda: s.post(B + f'/entry/{entry}/upload', files=[('file', f) for f in files], headers={'Referer': B + f'/entry/{entry}'}))
SRT = 'ep%d.srt' % random.randrange(10**6)
r = upload([(SRT, episode(300, '我们的征途是星辰大海。'), 'application/x-subrip')])
check('Chinese subtitles upload to a drama', any('successful' in f.lower() for f in flashes(r.text)), flashes(r.text))
check('the file is in the folder', os.path.isfile(os.path.join('W:/dunglive-local/subs/Donghua/Aishen Qiaokeli-ing', SRT)))
r = upload([('ja.srt', episode(300, '吾輩は猫である。名前はまだ無い。'), 'application/x-subrip')])
check('Japanese subtitles are refused on the Chinese site, and the reason speaks of dramas', any('not Chinese' in f and 'audiobook' not in f for f in flashes(r.text)), flashes(r.text))

print(f'{sum(results)} of {len(results)} pass')
sys.exit(0 if all(results) else 1)
