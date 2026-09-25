#!/usr/bin/env python3
"""Regenerate lang/fr/LC_MESSAGES/ricercar-ui.po from ui/*.slint + tools/fr.json.

Slint uses the enclosing component name as gettext context."""
import glob, json, os, re, sys

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.dirname(here)
fr = json.load(open(os.path.join(here, "fr.json"), encoding="utf-8"))
tr = re.compile(r'@tr\(\s*"((?:[^"\\]|\\.)*)"(?:\s*\|\s*"((?:[^"\\]|\\.)*)"\s*%)?')
comp = re.compile(r'^\s*(?:export\s+)?component\s+([A-Za-z_][\w-]*)', re.M)
entries, missing = {}, []
for f in sorted(glob.glob(os.path.join(root, "ui", "*.slint"))):
    text = open(f, encoding="utf-8").read()
    starts = [(m.start(), m.group(1)) for m in comp.finditer(text)]
    for m in tr.finditer(text):
        ctx = ""
        for pos, name in starts:
            if pos < m.start():
                ctx = name
        entries.setdefault((ctx, m.group(1), m.group(2)), None)
def esc(s): return s.replace("\\", "\\\\").replace('"', '\\"')
out = ['msgid ""', 'msgstr ""', '"Content-Type: text/plain; charset=UTF-8\\n"',
       '"Language: fr\\n"', '"Plural-Forms: nplurals=2; plural=(n > 1);\\n"', ""]
for ctx, sid, plural in entries:
    t = fr.get(sid)
    if t is None:
        missing.append(sid)
        continue
    out.append(f'msgctxt "{esc(ctx)}"')
    out.append(f'msgid "{esc(sid)}"')
    if plural:
        out.append(f'msgid_plural "{esc(plural)}"')
        out.append(f'msgstr[0] "{esc(t[0])}"')
        out.append(f'msgstr[1] "{esc(t[1])}"')
    else:
        out.append(f'msgstr "{esc(t)}"')
    out.append("")
dest = os.path.join(root, "lang", "fr", "LC_MESSAGES", "ricercar-ui.po")
os.makedirs(os.path.dirname(dest), exist_ok=True)
open(dest, "w", encoding="utf-8").write("\n".join(out))
if missing:
    print("untranslated:", *missing, sep="\n  ", file=sys.stderr)
    sys.exit(1)
print(f"{len(entries)} entries -> {dest}")
