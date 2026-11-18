#!/usr/bin/env python3
"""Convert Scoop bucket manifests into an AllStore custom JSON feed.

The output is a plain ``{"apps": [...]}`` document that AllStore can consume
in Settings -> Custom sources with **zero client changes**. Every entry that
Scoop publishes a SHA-256 for carries it as ``hash``, so the downloader
verifies the installer after it lands on disk.

Typical use (local checkout is fastest and works offline)::

    git clone --depth=1 https://github.com/ScoopInstaller/Main.git
    python tools/convert_scoop.py --bucket-dir ../Main/bucket --out scoop.json

Without ``--bucket-dir`` the script downloads the repository tarball from
codeload. Additional buckets can be supplied with repeated ``--repo`` or
``--bucket-dir`` flags::

    python tools/convert_scoop.py --repo Main --repo Extras --out scoop.json

Optional ``--head-check`` probes each URL and drops dead links. It is off by
default because many CDNs reject HEAD requests even though GET works.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import io
import json
import os
import re
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

ARCH_KEYS = ("64bit", "x86_64", "amd64")
SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")

# AllStore's category pages are built from the Lenovo sidebar vocabulary. A
# label outside this set renders in search but never appears under a category,
# so the converter only emits names from this list (see TENCNET_CATEGORIES in
# src/source.rs). Rules are ordered: the first tag match wins.
CATEGORY_RULES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("编程软件", ("development", "compiler", "programming", "git", "python", "nodejs", "sdk", "ide", "build")),
    ("安全防护", ("security", "antivirus", "privacy", "password", "firewall", "vpn")),
    ("解压刻录", ("compression", "archive", "zip", "7zip", "rar", "burn")),
    ("图片图形", ("image", "graphics", "photo", "design", "drawing", "screenshot", "icon")),
    ("剪辑工具", ("video", "media", "encoder", "ffmpeg", "streaming")),
    ("音乐软件", ("music", "audio", "player", "podcast")),
    ("浏览器", ("browser", "web", "http", "download-manager")),
    ("下载工具", ("download", "torrent", "ftp", "netdisk")),
    ("游戏", ("game", "gaming", "emulator", "steam")),
    ("办公教育", ("office", "education", "pdf", "note", "productivity", "language")),
    ("系统工具", ("system", "utility", "admin", "shell", "driver", "monitor", "cleaner", "backup")),
)

DEFAULT_CATEGORY = "系统工具"
TARBALL = "https://codeload.github.com/ScoopInstaller/{repo}/tar.gz/refs/heads/master"
USER_AGENT = "AllStore-Scoop-Converter/1.0 (+https://github.com/XINKEJU/AllStore)"


def request(url: str, method: str = "GET", timeout: int = 30):
    req = urllib.request.Request(url, method=method, headers={"User-Agent": USER_AGENT})
    return urllib.request.urlopen(req, timeout=timeout)


def fetch_manifests(repos: list[str]) -> tuple[list[tuple[str, dict]], list[str]]:
    """Return (name, manifest) pairs and non-fatal warnings."""
    out: list[tuple[str, dict]] = []
    warnings: list[str] = []
    for repo in repos:
        url = TARBALL.format(repo=repo)
        print(f"下载 {repo} bucket: {url}")
        try:
            with request(url, timeout=90) as resp:
                blob = resp.read()
        except (urllib.error.URLError, OSError) as exc:
            warnings.append(f"{repo}: 无法下载（{exc}）；可用 --bucket-dir 指定本地清单")
            continue
        found = 0
        with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as tar:
            for member in tar:
                if not member.isfile() or "/bucket/" not in member.name:
                    continue
                if not member.name.lower().endswith(".json"):
                    continue
                stream = tar.extractfile(member)
                if stream is None:
                    continue
                try:
                    data = json.load(stream)
                except (ValueError, UnicodeDecodeError):
                    continue
                if not isinstance(data, dict):
                    continue
                out.append((Path(member.name).stem, data))
                found += 1
        print(f"  {repo}: 读取 {found} 个清单")
        if found == 0:
            warnings.append(f"{repo}: 压缩包里没有 bucket/*.json")
    return out, warnings


def load_local(directories: list[str]) -> tuple[list[tuple[str, dict]], list[str]]:
    out: list[tuple[str, dict]] = []
    warnings: list[str] = []
    for raw in directories:
        directory = Path(raw).expanduser()
        if not directory.is_dir():
            warnings.append(f"{directory}: 目录不存在")
            continue
        found = 0
        for p in sorted(directory.glob("*.json")):
            try:
                data = json.loads(p.read_text(encoding="utf-8-sig"))
            except (ValueError, UnicodeDecodeError):
                continue
            if isinstance(data, dict):
                out.append((p.stem, data))
                found += 1
        print(f"读取本地清单 {directory}: {found} 个")
    return out, warnings


def pick(value):
    """Scoop allows a scalar or a list; take the first usable entry."""
    if isinstance(value, list):
        return next((v for v in value if isinstance(v, str) and v.strip()), None)
    if isinstance(value, str) and value.strip():
        return value.strip()
    return None


def category_of(manifest: dict) -> str:
    tags = manifest.get("tags") or []
    if isinstance(tags, str):
        tags = [tags]
    tags = [t.lower() for t in tags if isinstance(t, str)]
    for label, keywords in CATEGORY_RULES:
        if any(any(k in tag for k in keywords) for tag in tags):
            return label
    description = str(manifest.get("description") or "").lower()
    for label, keywords in CATEGORY_RULES:
        if any(k in description for k in keywords):
            return label
    return DEFAULT_CATEGORY


def convert(stem: str, m: dict) -> dict | None:
    arch = m.get("architecture") if isinstance(m.get("architecture"), dict) else {}
    chosen = next((arch[k] for k in ARCH_KEYS if isinstance(arch.get(k), dict)), {})

    def field(key: str):
        return pick(chosen.get(key)) or pick(m.get(key))

    url = field("url")
    if not url or not url.lower().startswith(("http://", "https://")):
        return None
    name = pick(m.get("name")) or stem
    version = field("version")
    if not version:
        return None

    hash_value = field("hash")
    if hash_value and not SHA256_RE.fullmatch(hash_value):
        hash_value = None  # never ship a digest that could never pass

    desc = pick(m.get("description"))
    if desc:
        desc = " ".join(desc.split())[:160]

    row = {
        "id": f"S:{name.lower()}",
        "name": name,
        "version": version,
        "url": url,
        "desc": desc or "Scoop 社区清单",
        "category": category_of(m),
    }
    if hash_value:
        row["hash"] = hash_value.lower()
    return row


def probe(row: dict, timeout: int) -> dict | None:
    try:
        with request(row["url"], method="HEAD", timeout=timeout) as resp:
            if resp.status >= 400:
                return None
            size = resp.headers.get("Content-Length")
            if size and size.isdigit() and 0 < int(size) < 4 * 1024**3:
                row["size"] = f"{int(size) / 1024 / 1024:.1f} MB"
    except (urllib.error.URLError, OSError, ValueError):
        return None
    return row


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", action="append", default=[], help="ScoopInstaller repo, repeatable (default: Main)")
    ap.add_argument("--bucket-dir", action="append", default=[], help="local bucket directory, repeatable")
    ap.add_argument("--out", default="scoop-allstore.json", help="output JSON path")
    ap.add_argument("--limit", type=int, default=0, help="keep at most N apps (0 = all)")
    ap.add_argument("--head-check", action="store_true", help="probe URLs with HEAD and drop dead links")
    ap.add_argument("--workers", type=int, default=12, help="HEAD check concurrency")
    ap.add_argument("--timeout", type=int, default=12, help="HEAD check timeout in seconds")
    ap.add_argument("--pretty", action="store_true", help="indent the output for review")
    args = ap.parse_args()

    if not args.bucket_dir and not args.repo:
        args.repo = ["Main"]

    manifests, warnings = load_local(args.bucket_dir)
    remote, more = fetch_manifests(args.repo)
    manifests.extend(remote)
    warnings.extend(more)

    rows: list[dict] = []
    seen: set[str] = set()
    for stem, manifest in manifests:
        row = convert(stem, manifest)
        if row is None:
            continue
        key = row["name"].lower()
        if key in seen:
            continue
        seen.add(key)
        rows.append(row)

    # Apply the limit before probing so a small sample is cheap to check.
    if args.limit > 0:
        rows = rows[: args.limit]

    if args.head_check:
        print(f"HEAD 探测 {len(rows)} 个下载地址（{args.workers} 并发）…")
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.workers)) as pool:
            checked = list(pool.map(lambda r: probe(r, args.timeout), rows))
        rows = [r for r in checked if r is not None]

    out = Path(args.out).expanduser()
    out.parent.mkdir(parents=True, exist_ok=True)
    payload = {"apps": rows}
    text = json.dumps(
        payload,
        ensure_ascii=False,
        indent=2 if args.pretty else None,
        separators=None if args.pretty else (",", ":"),
    )
    fd, tmp = tempfile.mkstemp(prefix=out.name + ".", suffix=".tmp", dir=out.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        os.replace(tmp, out)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)

    print(f"写出 {out}：{len(rows)} 款应用 / {len(text.encode('utf-8')) / 1024:.0f} KB")
    with_hash = sum("hash" in row for row in rows)
    print(f"其中 {with_hash} 款带 SHA-256，{len(rows) - with_hash} 款来源未提供摘要")
    for w in warnings:
        print(f"警告：{w}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
