#!/usr/bin/env python3
"""从本机 Firefox profile 导出 QQ 音乐 Cookie（无需手动复制粘贴）。

用法:
    python3 tools/firefox_cookie.py            # 打印 Cookie 串
    python3 tools/firefox_cookie.py --export   # 写入 ~/.config/qqmusic-dl/cookie.txt
    python3 tools/firefox_cookie.py --profile ~/.mozilla/firefox/xxx.default-release

只读取导出用的副本，不会改动原库。
"""
import argparse
import glob
import os
import shutil
import sqlite3
import sys
import tempfile

KEYS = ("qqmusic_key", "qm_keyst", "wxuin", "uin")


def find_profiles(explicit):
    if explicit:
        return [explicit]
    roots = [
        os.path.expanduser("~/.mozilla/firefox"),
        os.path.expanduser("~/.config/mozilla/firefox"),
        os.path.expanduser("~/snap/firefox/common/.mozilla/firefox"),
        os.path.expanduser("~/.var/app/org.mozilla.firefox/.mozilla/firefox"),
    ]
    out = []
    for root in roots:
        for path in glob.glob(os.path.join(root, "*", "cookies.sqlite")):
            if os.path.exists(os.path.join(os.path.dirname(path), "prefs.js")):
                out.append(os.path.dirname(path))
    return out


def read_cookies(profile):
    src = os.path.join(profile, "cookies.sqlite")
    if not os.path.exists(src):
        return {}
    tmp = os.path.join(tempfile.mkdtemp(prefix="ffcookie-"), "cookies.sqlite")
    shutil.copy2(src, tmp)
    db = sqlite3.connect(tmp)
    try:
        rows = db.execute(
            "select host, name, value from moz_cookies where host like '%qq.com'"
        ).fetchall()
    finally:
        db.close()
        shutil.rmtree(os.path.dirname(tmp), ignore_errors=True)

    picked = {}
    for host, name, value in rows:
        if name in KEYS and value and name not in picked:
            picked[name] = value
    return picked


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile")
    ap.add_argument("--export", action="store_true")
    args = ap.parse_args()

    profiles = find_profiles(args.profile)
    if not profiles:
        sys.exit("没找到 Firefox profile（可用 --profile 指定）")

    for profile in profiles:
        picked = read_cookies(profile)
        key = picked.get("qqmusic_key") or picked.get("qm_keyst")
        uin = picked.get("wxuin") or picked.get("uin")
        if not key or not uin:
            continue
        cookie = f"qm_keyst={key}; qqmusic_key={key}; wxuin={uin}; uin={uin}"
        print(f"# profile: {profile}", file=sys.stderr)
        if args.export:
            out = os.path.expanduser("~/.config/qqmusic-dl/cookie.txt")
            os.makedirs(os.path.dirname(out), exist_ok=True)
            with open(out, "w", encoding="utf-8") as fh:
                fh.write(cookie)
            os.chmod(out, 0o600)
            print(f"已写入 {out}", file=sys.stderr)
        else:
            print(cookie)
        return

    sys.exit("这些 profile 里没有 QQ 音乐登录票据（先在 Firefox 登录 y.qq.com）")


if __name__ == "__main__":
    main()
