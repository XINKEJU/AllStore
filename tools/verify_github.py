"""Resolve every candidate repo's latest Windows installer from github.com HTML.

No API token involved: `/releases/latest` redirects to the tag page, and the
asset list lives in the `releases/expanded_assets/<tag>` fragment. Two plain
GETs per repo, so the 60/h anonymous API quota is never touched.
"""
import json
import re
import sys
import time
import urllib.parse
import urllib.request
import concurrent.futures as cf

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120 Safari/537.36"
HARD_BAD = (".sig", ".sha", ".asc", "checksums", "arm64", "arm-64", "arm_64", "aarch64",
            "armv7", "win-arm", "windows-arm", "symbols", "debug", ".pdb", ".nupkg",
            ".zip", ".7z", ".msix", ".txt", ".json", ".blockmap")
GOOD_ARCH = ("x64", "amd64", "win64", "x86_64", "x86-64", "64-bit", "64bit")
BAD_ARCH = ("win32", "i386", "i686", "ia32", "32bit", "32-bit", "x86.exe", "x86.msi")
GOOD_WORD = ("setup", "installer", "install")
BAD_WORD = ("portable", "-cli", "_cli", "patch", "update-only")


def _alnum(s):
    return "".join(c for c in s.lower() if c.isalnum())


def score(name, app=""):
    low = name.lower()
    if any(b in low for b in HARD_BAD):
        return None
    if not (low.endswith(".exe") or low.endswith(".msi")):
        return None
    if re.search(r"\dto\d+", low):  # Playnite-style "1061to1064.exe" patcher
        return None
    s = 0.0
    if any(g in low for g in GOOD_ARCH):
        s += 4
    if any(b in low for b in BAD_ARCH):
        s -= 5
    if any(g in low for g in GOOD_WORD):
        s += 3
    if low.endswith(".msi"):
        s += 1
    if app and _alnum(app) and _alnum(app) in _alnum(low):
        s += 3
    if any(b in low for b in BAD_WORD):
        s -= 3
    s -= len(low) / 200.0
    return s


def get(url):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=90) as r:
        return r.geturl(), r.read()


def via_api(repo, app):
    """One request, ~0.8 s, but the anonymous quota is 60/h."""
    req = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/releases/latest",
        headers={"User-Agent": "AllStore", "Accept": "application/vnd.github+json"})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            d = json.load(r)
    except Exception as e:  # noqa: BLE001 - fall back to scraping
        return None, f"{type(e).__name__}: {e}"
    assets = d.get("assets") or []
    best, best_s = None, None
    for a in assets:
        sc = score(a["name"], app)
        if sc is None:
            continue
        if best_s is None or sc > best_s:
            best, best_s = a, sc
    if not best:
        return None, f"no exe among {len(assets)} assets"
    return {"tag": d["tag_name"], "asset": best["name"],
            "url": best["browser_download_url"]}, None


def via_html(repo, app):
    final, body = get(f"https://github.com/{repo}/releases/latest")
    m = re.search(r"/releases/tag/([^\"]+)", final)
    if not m:
        m = re.search(r"/releases/tag/([^\"]+)", body.decode("utf-8", "ignore"))
    if not m:
        return None, "no tag"
    tag = urllib.parse.unquote(m.group(1))
    _, frag = get(f"https://github.com/{repo}/releases/expanded_assets/{urllib.parse.quote(tag)}")
    html = frag.decode("utf-8", "ignore")
    best, best_s = None, None
    for href in set(re.findall(r'href="(/[^"]+/releases/download/[^"]+)"', html)):
        href = urllib.parse.unquote(href)
        sc = score(href.rsplit("/", 1)[-1], app)
        if sc is None:
            continue
        if best_s is None or sc > best_s:
            best, best_s = href, sc
    if not best:
        return None, "no exe among assets"
    return {"tag": tag, "asset": best.rsplit("/", 1)[-1],
            "url": "https://github.com" + best}, None


def resolve(repo, app=""):
    last = ""
    d, err = via_api(repo, app)
    if d:
        return repo, d, None
    last = err or ""
    if "no exe" in last:
        return repo, None, last
    time.sleep(1.0)
    try:
        d, err = via_html(repo, app)
        return repo, d, err
    except Exception as e:  # noqa: BLE001 - report and continue
        return repo, None, f"{type(e).__name__}: {e}"


def main():
    cands = json.load(open(sys.argv[1], encoding="utf-8"))
    repos = [c[0] for c in cands]
    names = {c[0]: c[1] for c in cands}
    out, bad = [], []
    try:
        prev = {x["repo"]: x for x in json.load(open(sys.argv[2], encoding="utf-8"))}
    except Exception:  # noqa: BLE001 - first run
        prev = {}
    t = time.time()
    with cf.ThreadPoolExecutor(3) as ex:
        todo = [r for r in repos if r not in prev]
        results = {r: (d, e) for r, d, e in ex.map(lambda r: resolve(r, names[r]), todo)}
        for r, x in prev.items():
            results[r] = ({"tag": x["version"], "asset": x["asset"], "url": ""}, None)
    for repo, name, cat, desc in cands:
        d, err = results[repo]
        if d:
            out.append({"repo": repo, "name": name, "cat": cat, "desc": desc,
                        "version": d["tag"], "asset": d["asset"]})
        else:
            bad.append((repo, err))
    json.dump(out, open(sys.argv[2], "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    print(f"resolved {len(out)}/{len(cands)} in {time.time()-t:.0f}s")
    for repo, err in bad:
        print("  MISS", repo, "-", err)


if __name__ == "__main__":
    main()
