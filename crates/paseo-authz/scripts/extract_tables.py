#!/usr/bin/env python3
"""Regenerate tables/{inbound,outbound}.json from the TS authorization source.

usage: extract_tables.py [TS_CHECKOUT]
Requirement encoding (mirrors TS PermissionRequirement):
  null -> "always", "p" -> {"one": "p"}, ["a","b"] -> {"any": ["a","b"]}
Entry order is the TS source order (kept for diffing); the Rust build sorts.
"""
import json, re, sys, os

ts = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/Documents/AI/paseo-vietnamese-voice")
src = open(f"{ts}/packages/server/src/server/authorization/operation-permissions.ts").read()
here = os.path.dirname(os.path.abspath(__file__))

def block(name):
    m = re.search(r"const %s = \{\n(.*?)\n\} as const satisfies" % name, src, re.S)
    out = []
    for line in m.group(1).splitlines():
        line = line.strip()
        if not line:
            continue
        m2 = re.match(r'^(?:"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*)): (.+),$', line)
        assert m2, line
        key = m2.group(1) or m2.group(2)
        v = m2.group(3)
        if v == "null":
            req = "always"
        elif v.startswith("["):
            req = {"any": json.loads(v)}
        else:
            req = {"one": json.loads(v)}
        out.append([key, req])
    keys = [k for k, _ in out]
    assert len(keys) == len(set(keys)), "duplicate keys"
    return out

for n, f in (("INBOUND_PERMISSION", "inbound"), ("OUTBOUND_PERMISSION", "outbound")):
    t = block(n)
    json.dump(t, open(f"{here}/../tables/{f}.json", "w"), indent=0)
    open(f"{here}/../tables/{f}.json", "a").write("\n")
    print(f, len(t))
