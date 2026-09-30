#!/usr/bin/env python3
"""Regenerate lang/<code>/LC_MESSAGES/ricercar-ui.po from ui/*.slint and
tools/<code>.json, for every language listed in LANGUAGES.

Slint uses the enclosing component name as gettext context. The JSON files
also hold the Rust-side strings (text::t), which the .po files skip."""
import glob, json, os, re, sys

# code -> gettext plural rule. A plural entry in <code>.json is a list with
# one string per form, in the order of the rule.
LANGUAGES = {
    "fr": "nplurals=2; plural=(n > 1);",
    "de": "nplurals=2; plural=(n != 1);",
    "es": "nplurals=2; plural=(n != 1);",
    "it": "nplurals=2; plural=(n != 1);",
    "ja": "nplurals=1; plural=0;",
}

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.dirname(here)
tr = re.compile(r'@tr\(\s*"((?:[^"\\]|\\.)*)"(?:\s*\|\s*"((?:[^"\\]|\\.)*)"\s*%)?')
comp = re.compile(r'^\s*(?:export\s+)?component\s+([A-Za-z_][\w-]*)', re.M)
entries = {}
for f in sorted(glob.glob(os.path.join(root, "ui", "*.slint"))):
    text = open(f, encoding="utf-8").read()
    starts = [(m.start(), m.group(1)) for m in comp.finditer(text)]
    for m in tr.finditer(text):
        ctx = ""
        for pos, name in starts:
            if pos < m.start():
                ctx = name
        entries.setdefault((ctx, m.group(1), m.group(2)), None)


def esc(s):
    return s.replace("\\", "\\\\").replace('"', '\\"')


failed = False
for code, rule in LANGUAGES.items():
    table = json.load(open(os.path.join(here, f"{code}.json"), encoding="utf-8"))
    nplurals = int(re.search(r"nplurals=(\d+)", rule).group(1))
    out = ['msgid ""', 'msgstr ""', '"Content-Type: text/plain; charset=UTF-8\\n"',
           f'"Language: {code}\\n"', f'"Plural-Forms: {rule}\\n"', ""]
    missing = []
    for ctx, sid, plural in entries:
        t = table.get(sid)
        if t is None or (plural and (not isinstance(t, list) or len(t) != nplurals)):
            missing.append(sid)
            continue
        out.append(f'msgctxt "{esc(ctx)}"')
        out.append(f'msgid "{esc(sid)}"')
        if plural:
            out.append(f'msgid_plural "{esc(plural)}"')
            for i, form in enumerate(t):
                out.append(f'msgstr[{i}] "{esc(form)}"')
        else:
            out.append(f'msgstr "{esc(t)}"')
        out.append("")
    dest = os.path.join(root, "lang", code, "LC_MESSAGES", "ricercar-ui.po")
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    open(dest, "w", encoding="utf-8").write("\n".join(out))
    if missing:
        print(f"{code}: untranslated:", *missing, sep="\n  ", file=sys.stderr)
        failed = True
    else:
        print(f"{code}: {len(entries)} entries -> {dest}")
sys.exit(1 if failed else 0)
