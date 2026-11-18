#!/usr/bin/env python3
"""Build an AllStore custom-source feed from Microsoft Store WPM apps.

WPM = non-packaged Win32 software published on the Store. Those cards expose
a real ``InstallerUrl`` + SHA-256 through ``packageManifests``; MSIX/APPX rows
are skipped because AllStore cannot deploy packages.

The generated feed stores only ``ms:PRODUCTID`` — AllStore resolves the
current installer (and its hash) at download time, so a pinned URL never goes
stale and the feed stays small. Add the output under Settings -> Custom
sources; no client changes are required.

    python tools/convert_msstore.py --out dist/msstore-wpm.json

Use ``--fetch-versions`` to attach the version shown on the card (one extra
request per app). ``--queries-file`` lets you replace the built-in keyword
list with one keyword per line, e.g. to target a different market/locale.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import re
import sys
import urllib.parse
import urllib.request
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

BASE = "https://storeedgefd.dsx.mp.microsoft.com/v9.0"
HEADERS = {
    "User-Agent": "WindowsStore/22106.1401.2.0",
    "Accept": "*/*",
    "Accept-Language": "en-US",
}

# One query per well-known title: the search endpoint returns up to 20 cards,
# of which usually one or two are WPM. Broad generic words waste requests.
DEFAULT_QUERIES = (
    "7zip", "VLC", "Firefox", "Chrome", "Brave", "Opera", "Edge",
    "PotPlayer", "GIMP", "Inkscape", "Blender", "Audacity", "OBS Studio",
    "foobar2000", "MusicBee", "iTunes", "Kodi", "HandBrake", "IrfanView",
    "Paint.NET", "Notepad++", "Visual Studio Code", "Git", "PuTTY", "WinSCP",
    "FileZilla", "Telegram", "Discord", "Zoom", "Skype", "Thunderbird",
    "LibreOffice", "Python", "Java", "Node.js", "Docker", "VirtualBox",
    "qBittorrent", "Transmission", "Everything", "Process Explorer",
    "CPU-Z", "GPU-Z", "HWMonitor", "CrystalDiskInfo", "Beyond Compare",
    "Steam", "Epic Games", "Xmind", "Typora", "Obsidian", "Joplin",
    "Wireshark", "Fiddler", "Postman", "DBeaver", "Mozilla Thunderbird",
    "VLC media player", "PDF reader", "Screenshot", "Screen recorder",
)

# Store categories are English phrases; AllStore's sidebar uses Lenovo's
# Chinese vocabulary. A row with an unknown label never shows on a category
# page, so always fall back to a valid one (see TENCNET_CATEGORIES).
CATEGORY_RULES = (
    ("编程软件", ("developer", "programming", "coding", "code", "software development")),
    ("安全防护", ("security", "antivirus", "privacy", "password", "firewall")),
    ("视频软件", ("photo & video", "video", "movie", "television")),
    ("音乐软件", ("music", "audio", "podcast", "radio")),
    ("图片图形", ("graphics", "design", "photo", "image", "drawing")),
    ("浏览器", ("browser", "web", "internet")),
    ("下载工具", ("download", "file transfer", "torrent", "ftp")),
    ("游戏", ("game", "gaming", "xbox", "entertainment")),
    ("办公教育", ("productivity", "office", "education", "business", "notes", "pdf")),
    ("系统工具", ("utilities", "system", "tools", "desktop", "performance")),
)
PID_RE = re.compile(r"^[A-Z0-9]{8,32}$")


def request_json(url: str, timeout: int) -> dict:
    req = urllib.request.Request(url, headers=HEADERS)
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


def search(query: str, market: str, locale: str, timeout: int) -> list[dict]:
    url = (
        f"{BASE}/search?query={urllib.parse.quote(query)}&market={market}"
        f"&locale={locale}&deviceFamily=Windows.Desktop&mediaType=All&filters="
    )
    try:
        data = request_json(url, timeout)
    except Exception as exc:  # one bad query must not kill the whole feed
        print(f"  ! {query}: {exc}", file=sys.stderr)
        return []
    return (data.get("Payload") or {}).get("SearchResults") or []


def to_row(card: dict) -> dict | None:
    install = card.get("Installer") or {}
    if install.get("Type") != "WPM":
        return None
    pid = card.get("ProductId") or ""
    title = (card.get("Title") or "").strip()
    if not PID_RE.fullmatch(pid) or not title:
        return None
    logo = ""
    for image in card.get("Images") or []:
        url = image.get("Url") or ""
        if image.get("ImageType") == "logo" and url.startswith("https://"):
            logo = url
            break
    desc = (card.get("Description") or card.get("LongDescription") or "").strip()
    if len(desc) > 200:
        desc = desc[:197].rstrip() + "..."
    categories = " ".join(card.get("Categories") or []).lower()
    category = next(
        (label for label, keys in CATEGORY_RULES if any(k in categories for k in keys)),
        "系统工具",
    )
    row: dict[str, str] = {
        "name": title,
        "url": f"ms:{pid}",
        "category": category,
    }
    if logo:
        row["logo"] = logo
    if desc:
        row["desc"] = desc
    return row


def fetch_version(pid: str, timeout: int) -> str:
    try:
        data = request_json(f"{BASE}/packageManifests/{pid}", timeout)
    except Exception:
        return ""
    versions = ((data.get("Data") or {}).get("Versions") or [])
    if not versions:
        return ""
    first = versions[0]
    return str(first.get("PackageVersion") or (first.get("DefaultLocale") or {}).get("PackageVersion") or "")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default="dist/msstore-wpm.json")
    parser.add_argument("--queries", action="append", default=[], help="repeatable; replaces defaults when given")
    parser.add_argument("--queries-file", help="one keyword per line")
    parser.add_argument("--market", default="US")
    parser.add_argument("--locale", default="en-US")
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--timeout", type=int, default=20)
    parser.add_argument("--fetch-versions", action="store_true")
    args = parser.parse_args()

    queries = list(args.queries)
    if args.queries_file:
        queries += [line.strip() for line in Path(args.queries_file).read_text(encoding="utf-8").splitlines() if line.strip() and not line.startswith("#")]
    if not queries:
        queries = list(DEFAULT_QUERIES)

    print(f"搜索 {len(queries)} 个关键词（{args.market}/{args.locale}）…")
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.workers)) as pool:
        cards = list(pool.map(lambda q: search(q, args.market, args.locale, args.timeout), queries))
    by_id: dict[str, dict] = {}
    for batch in cards:
        for card in batch:
            row = to_row(card)
            if row:
                by_id.setdefault(row["url"], row)
    rows = sorted(by_id.values(), key=lambda r: r["name"].lower())
    print(f"找到 {len(rows)} 款可直接安装的 Win32 应用")

    if args.fetch_versions and rows:
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.workers)) as pool:
            versions = list(pool.map(lambda r: fetch_version(r["url"][3:], args.timeout), rows))
        for row, version in zip(rows, versions):
            if version:
                row["version"] = version

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    feed = {
        "name": "微软商店（Win32）",
        "description": "Microsoft Store 的非打包 Win32 应用；安装包与 SHA-256 在下载时从商店清单解析",
        "apps": rows,
    }
    out.write_text(json.dumps(feed, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"已写入 {out}（{round(out.stat().st_size / 1024)} KB）")
    return 0 if rows else 1


if __name__ == "__main__":
    raise SystemExit(main())
