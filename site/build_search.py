#!/usr/bin/env python3
"""Builds site/search.json, the index behind the site's search (Cmd+K or Ctrl+K).

Rebuild it, from the repository root, with one command:

    python3 site/build_search.py

Commit the new site/search.json with the change that made it stale: any change to the Learn
pages, the landing page or the public API.

What it indexes:

- Learn: every chapter in site/learn/, every section and subsection with an id (the result opens
  that exact heading), and the text under each, code included.
- The landing page: every section, and every card, point and note in it.
- The API: every public item of the crate, its methods, associated constants, variants and
  fields, each linked to its page on https://docs.rs/nuclei/latest/nuclei/.

Where the API comes from. The public names come from the source: src/lib.rs, its re-exports and
the public modules under src/, read with their cfg attributes evaluated for each platform. The
pages, kinds, anchors and summaries come from rustdoc, which the script runs for the target
docs.rs builds (x86_64-unknown-linux-gnu, default features), so every link is the page docs.rs
serves and never a guess. The two must agree: a name public in the source that rustdoc does not
document, or the other way round, stops the build. A name public only on another platform (the
Windows console events) is indexed from the source with its platform, and links to the docs.rs
page of the nearest module docs.rs has.

Options:

    --doc DIR   use the rustdoc output in DIR (the folder holding nuclei/) instead of running
                cargo doc; it must be a build for x86_64-unknown-linux-gnu with default features
"""

import argparse
import html
import json
import re
import subprocess
import sys
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
OUT = SITE / "search.json"
DOCS_TARGET = "x86_64-unknown-linux-gnu"
DEFAULT_FEATURES = {"iouring", "epoll", "attributes"}


def fail(message):
    sys.exit(f"build_search: {message}")


def squash(text):
    return re.sub(r"\s+", " ", text).strip()


# Rust source: the public names of every public module, per platform. ------------------------

PLATFORMS = {
    "linux": {"unix": True, "windows": False, "target_os": "linux", "target_family": "unix"},
    "macos": {"unix": True, "windows": False, "target_os": "macos", "target_family": "unix"},
    "windows": {"unix": False, "windows": True, "target_os": "windows", "target_family": "windows"},
}
PLATFORM_NAMES = {"linux": "Linux", "macos": "macOS", "windows": "Windows"}

TOKEN = re.compile(
    r"""
    (?P<space>\s+)
  | (?P<doc>///(?!/)[^\n]*)
  | (?P<comment>//[^\n]*)
  | (?P<block>/\*)
  | (?P<str>b?r(?P<hashes>\#*)"(?:.|\n)*?"(?P=hashes))
  | (?P<plain>b?"(?:\\.|[^"\\])*")
  | (?P<char>b?'(?:\\.|[^'\\])')
  | (?P<life>'[A-Za-z_][A-Za-z0-9_]*)
  | (?P<ident>(?:r\#)?[A-Za-z_][A-Za-z0-9_]*)
  | (?P<num>[0-9][A-Za-z0-9_.]*)
  | (?P<punct>::|->|=>|.)
    """,
    re.VERBOSE | re.DOTALL,
)


def tokens(source):
    """The tokens of a Rust file as (kind, text) pairs; comments dropped, outer docs kept."""
    out, pos = [], 0
    while pos < len(source):
        match = TOKEN.match(source, pos)
        if match is None:
            fail(f"cannot read Rust source at offset {pos}")
        kind = match.lastgroup
        if kind == "block":
            depth, end = 1, match.end()
            while depth:
                opened, closed = source.find("/*", end), source.find("*/", end)
                if closed < 0:
                    fail("unterminated block comment in Rust source")
                if 0 <= opened < closed:
                    depth, end = depth + 1, opened + 2
                else:
                    depth, end = depth - 1, closed + 2
            pos = end
            continue
        if kind == "hashes":
            kind = "str"
        if kind not in ("space", "comment"):
            out.append((kind if kind != "plain" else "str", match.group()))
        pos = match.end()
    return out


def cfg_true(tokens_, platform):
    """Evaluates a cfg predicate given as tokens, for one platform with default features."""
    facts = PLATFORMS[platform]
    pos = 0

    def predicate():
        nonlocal pos
        kind, name = tokens_[pos]
        pos += 1
        if name in ("all", "any", "not") and tokens_[pos][1] == "(":
            pos += 1
            values = []
            while tokens_[pos][1] != ")":
                values.append(predicate())
                if tokens_[pos][1] == ",":
                    pos += 1
            pos += 1
            if name == "all":
                return all(values)
            if name == "any":
                return any(values)
            return not values[0]
        if pos < len(tokens_) and tokens_[pos][1] == "=":
            value = tokens_[pos + 1][1].strip('"')
            pos += 2
            if name == "feature":
                return value in DEFAULT_FEATURES
            if name in facts:
                return facts[name] == value
            if name == "target_pointer_width":
                return value == "64"
            fail(f"unknown cfg key {name!r}: teach build_search.py its value")
        if name in ("unix", "windows"):
            return facts[name]
        if name in ("test", "loom", "nuclei_nightly", "miri"):
            return False
        if name in ("doc", "debug_assertions"):
            return True
        fail(f"unknown cfg name {name!r}: teach build_search.py its value")

    return predicate()


class Item:
    """A public name declared or re-exported in a public module of the source."""

    def __init__(self, name, keyword, platforms, doc):
        self.name, self.keyword, self.platforms, self.doc = name, keyword, platforms, doc
        self.methods = []


class SourceModule:
    def __init__(self, path, platforms):
        self.path, self.platforms = path, platforms
        self.items = {}
        self.children = []


def first_paragraph(doc_lines):
    paragraph = []
    for line in doc_lines:
        line = line[3:].strip()
        if not line:
            if paragraph:
                break
            continue
        paragraph.append(line)
    return squash(" ".join(paragraph))


def skip_balanced(toks, pos):
    """From an opening bracket, the position after its match."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[toks[pos][1]]]
    pos += 1
    while stack:
        text = toks[pos][1]
        if text in pairs:
            stack.append(pairs[text])
        elif text == stack[-1]:
            stack.pop()
        pos += 1
    return pos


def skip_item(toks, pos, braced):
    """The position after an item: its `;`, or its first `{...}` block when `braced`."""
    while True:
        text = toks[pos][1]
        if text == ";":
            return pos + 1
        if text == "{" and braced:
            return skip_balanced(toks, pos)
        if text in "([{":
            pos = skip_balanced(toks, pos)
        else:
            pos += 1


def use_names(toks, pos):
    """The names a `use` tree brings in, and the position after its `;`."""
    names = []

    def tree():
        nonlocal pos
        last = None
        while True:
            kind, text = toks[pos]
            if text == "{":
                pos += 1
                while toks[pos][1] != "}":
                    tree()
                    if toks[pos][1] == ",":
                        pos += 1
                pos += 1
                return
            if text == "*":
                fail("a glob re-export cannot be listed: name its items")
            if text == "as":
                names.append(toks[pos + 1][1])
                pos += 2
                return
            if text in (",", "}", ";"):
                if last is not None:
                    names.append(last)
                return
            if text != "::":
                last = text
            pos += 1

    tree()
    return names, skip_item(toks, pos, False)


def parse_module(path_parts, toks, pos, end, module, files_dir, own_dir):
    """Reads one module's items between pos and end into `module`."""
    while pos < end:
        docs, cfgs, hidden = [], [], False
        while pos < end and (toks[pos][0] == "doc" or toks[pos][1] == "#"):
            if toks[pos][0] == "doc":
                docs.append(toks[pos][1])
                pos += 1
                continue
            inner = toks[pos + 1][1] == "!"
            start = pos + (2 if inner else 1)
            close = skip_balanced(toks, start)
            attr = toks[start + 1 : close - 1]
            if not inner:
                if attr and attr[0][1] == "cfg":
                    cfgs.append(attr[2:-1])
                elif attr and attr[0][1] == "doc" and any(t[1] == "hidden" for t in attr):
                    hidden = True
            pos = close
        if pos >= end:
            break
        platforms = {p for p in module.platforms if all(cfg_true(c, p) for c in cfgs)}
        public = toks[pos][1] == "pub" and toks[pos + 1][1] != "("
        if toks[pos][1] == "pub":
            pos += 1
            if toks[pos][1] == "(":
                pos = skip_balanced(toks, pos)
        # Qualifiers before the keyword: `const fn`, `async fn`, `unsafe impl`, `extern "C" fn`.
        # A `const` followed by its name and a type is the item itself.
        while toks[pos][1] in ("const", "async", "unsafe", "extern"):
            if toks[pos][1] == "const" and toks[pos + 2][1] == ":":
                break
            pos += 2 if toks[pos][1] == "extern" and toks[pos + 1][0] == "str" else 1
        keyword = toks[pos][1]
        visible = public and platforms and not hidden
        if keyword == "mod":
            name = toks[pos + 1][1]
            if toks[pos + 2][1] == ";":
                pos += 3
                if visible:
                    child_file = files_dir / f"{name}.rs"
                    if not child_file.exists():
                        child_file = files_dir / name / "mod.rs"
                    child = SourceModule(path_parts + [name], platforms)
                    module.items[name] = Item(name, "mod", platforms, first_paragraph(docs))
                    module.children.append(child)
                    read_module_file(child, child_file)
            else:
                close = skip_balanced(toks, pos + 2)
                if visible:
                    child = SourceModule(path_parts + [name], platforms)
                    module.items[name] = Item(name, "mod", platforms, first_paragraph(docs))
                    module.children.append(child)
                    parse_module(child.path, toks, pos + 3, close - 1, child, own_dir / name, own_dir / name)
                pos = close
        elif keyword == "use":
            names, pos = use_names(toks, pos + 1)
            if visible:
                for name in names:
                    module.items[name] = Item(name, "use", platforms, "")
        elif keyword == "impl":
            close = skip_item(toks, pos, True)
            header = [t[1] for t in toks[pos:close]]
            brace = header.index("{")
            if "for" not in header[:brace] and platforms:
                self_type = header[brace - 1] if header[brace - 1] != ">" else None
                for name, item in module.items.items():
                    if self_type == name:
                        item.methods += impl_methods(toks, pos + brace + 1, close - 1, platforms)
            pos = close
        elif keyword == "macro_rules":
            pos = skip_balanced(toks, pos + 3)
            if toks[pos][1] == ";":
                pos += 1
        elif keyword in ("struct", "enum", "union", "trait", "fn", "type", "const", "static"):
            name = toks[pos + 1][1] if toks[pos + 1][1] != "mut" else toks[pos + 2][1]
            braced = keyword in ("struct", "enum", "union", "trait", "fn")
            pos = skip_item(toks, pos, braced)
            if visible and name != "_":
                module.items[name] = Item(name, keyword, platforms, first_paragraph(docs))
        elif toks[pos][0] == "ident" and toks[pos + 1][1] == "!":
            pos = skip_balanced(toks, pos + 2)
            if pos < end and toks[pos][1] == ";":
                pos += 1
        elif keyword == ";":
            pos += 1
        else:
            pos = skip_item(toks, pos, True)


def impl_methods(toks, pos, close, platforms):
    """The public functions of an impl block's body between pos and close: (name, doc, platforms).

    A macro invocation in the body (`loom_const_fn! { pub fn new ... }`) is read as more members.
    """
    out = []
    while pos < close:
        docs, cfgs = [], []
        while pos < close and (toks[pos][0] == "doc" or toks[pos][1] == "#"):
            if toks[pos][0] == "doc":
                docs.append(toks[pos][1])
                pos += 1
                continue
            end = skip_balanced(toks, pos + 1)
            attr = toks[pos + 2 : end - 1]
            if attr and attr[0][1] == "cfg":
                cfgs.append(attr[2:-1])
            pos = end
        if pos >= close:
            break
        here = {p for p in platforms if all(cfg_true(c, p) for c in cfgs)}
        if toks[pos][0] == "ident" and toks[pos + 1][1] == "!":
            body_end = skip_balanced(toks, pos + 2)
            out += impl_methods(toks, pos + 3, body_end - 1, here)
            pos = body_end + (1 if body_end < close and toks[body_end][1] == ";" else 0)
            continue
        public = toks[pos][1] == "pub" and toks[pos + 1][1] != "("
        scan = pos
        while toks[scan][1] not in ("fn", ";", "{", "="):
            scan += 1
        if toks[scan][1] == "fn":
            if public and here:
                out.append((toks[scan + 1][1], first_paragraph(docs), here))
            pos = skip_item(toks, scan, True)
        else:
            pos = skip_item(toks, pos, False)
    return out


def read_module_file(module, file):
    toks = tokens(file.read_text())
    if file.name in ("lib.rs", "mod.rs"):
        files_dir = own_dir = file.parent
    else:
        files_dir = own_dir = file.parent / file.stem
    parse_module(module.path, toks, 0, len(toks), module, files_dir, own_dir)


# A `macro_rules!` with the attributes and doc lines right above it.
MACRO_DEF = re.compile(r"(?P<attrs>(?:[ \t]*(?:///[^\n]*|#\[[^\n]*\])\n)*)[ \t]*macro_rules!\s*(?P<name>\w+)")


def source_modules():
    """Every public module of the crate, keyed by its path below `nuclei` ("" for the root)."""
    root = SourceModule([], set(PLATFORMS))
    read_module_file(root, ROOT / "src" / "lib.rs")
    # A `#[macro_export]` macro is a public name of the crate root wherever it is written, in a
    # private or hidden module too, which the walk above does not read.
    for file in sorted((ROOT / "src").rglob("*.rs")):
        for m in MACRO_DEF.finditer(file.read_text()):
            attrs = m.group("attrs")
            if "#[macro_export]" in attrs and "doc(hidden)" not in attrs:
                docs = [line.strip() for line in attrs.splitlines() if line.strip().startswith("///")]
                root.items[m.group("name")] = Item(m.group("name"), "macro", set(PLATFORMS), first_paragraph(docs))
    out, stack = {}, [root]
    while stack:
        module = stack.pop()
        out["::".join(module.path)] = module
        stack += module.children
    return out


# rustdoc: the pages, kinds, anchors and summaries docs.rs serves. -----------------------------

KINDS = {
    "struct": "struct",
    "enum": "enum",
    "trait": "trait",
    "fn": "fn",
    "constant": "const",
    "static": "static",
    "type": "type",
    "union": "union",
    "macro": "macro",
    "attr": "attr",
    "derive": "derive",
    "mod": "mod",
}


def text_of(fragment):
    # A where clause is a block of its own in rustdoc's headers, and the notable traits a
    # function returns sit behind a marker; neither reads as part of a signature.
    fragment = re.sub(r'<a href="#" class="tooltip"[^>]*>.*?</a>', "", fragment, flags=re.S)
    fragment = fragment.replace('<div class="where">', " ")
    return squash(html.unescape(re.sub(r"<[^>]+>", "", fragment))).rstrip(",")


def run_rustdoc():
    target_dir = ROOT / "target"
    command = ["cargo", "doc", "--no-deps", "--lib", "-p", "nuclei", "--target", DOCS_TARGET]
    print("build_search: " + " ".join(command), file=sys.stderr)
    result = subprocess.run(command, cwd=ROOT)
    if result.returncode != 0:
        fail(f"cargo doc failed; is the {DOCS_TARGET} standard library installed (rustup target add {DOCS_TARGET})?")
    return target_dir / DOCS_TARGET / "doc"


class DocCrate:
    """The rustdoc output for the crate: modules, items and the members of each item."""

    def __init__(self, doc_dir):
        self.base = doc_dir / "nuclei"
        if not (self.base / "index.html").exists():
            fail(f"{self.base} holds no rustdoc output")
        self.modules = {}
        self.items = {}
        self.read_module("", "index.html")

    def page(self, rel):
        path = self.base / rel
        if not path.exists():
            fail(f"rustdoc has no page {rel}")
        content = path.read_text()
        if 'http-equiv="refresh"' in content[:400]:
            fail(f"rustdoc page {rel} is a redirect, not the item's page")
        return content

    def read_module(self, mod_path, rel):
        content = self.page(rel)
        here = rel.rsplit("/", 1)[0] + "/" if "/" in rel else ""
        names = {}
        for match in re.finditer(r'<dt(?: id="reexport\.([^"]+)")?>(.*?)</dt>(?:<dd>(.*?)</dd>)?', content, re.S):
            reexport, body, summary = match.group(1), match.group(2), match.group(3) or ""
            link = re.search(r'<a class="(\w+)" href="([^"]+)" title="(\w+) ([^"]+)"', body)
            if link is None:
                fail(f"cannot read the entry {text_of(body)!r} on {rel}")
            css, href, kind, path = link.groups()
            target = normalize(here + href)
            if reexport:
                names[reexport] = ("use", target)
                self.items.setdefault(target, {"aliases": set()})["aliases"].add(
                    "nuclei::" + (mod_path + "::" if mod_path else "") + reexport
                )
                continue
            name = path.rsplit("::", 1)[-1]
            names[name] = (kind, target)
            entry = self.items.setdefault(target, {"aliases": set()})
            entry.update(kind=KINDS.get(kind, kind), path=path, summary=text_of(summary), module=mod_path)
            if kind == "mod":
                self.read_module(path.split("::", 1)[1] if "::" in path else "", target)
        self.modules[mod_path] = names

    def members(self, target):
        """The documented members of an item page: methods, constants, variants, fields."""
        content = self.page(target)
        out = []
        start = content.find('id="implementations"')
        stops = [content.find(f'id="{s}"') for s in ("trait-implementations", "synthetic-implementations", "blanket-implementations")]
        stops = [s for s in stops if s > start]
        inherent = content[start : min(stops)] if start >= 0 and stops else (content[start:] if start >= 0 else "")
        impl_type, impl_types = None, set()
        for match in re.finditer(r'<section id="([^"]+)" class="(impl|method|associatedconstant|associatedtype)"[^>]*>(.*?)</section>', inherent, re.S):
            anchor, css, body = match.groups()
            header = re.search(r'<h[34] class="code-header">(.*?)</h[34]>', body, re.S)
            signature = text_of(header.group(1)) if header else ""
            if css == "impl":
                impl_type = self_type(signature)
                impl_types.add(impl_type)
                continue
            out.append((anchor, signature, impl_type, doc_after(inherent, match.end())))
        if "/trait." in "/" + target:
            for match in re.finditer(r'<section id="((?:ty)?method\.[^"]+|associatedconstant\.[^"]+|associatedtype\.[^"]+)" class="method"[^>]*>(.*?)</section>', content[: start if start >= 0 else len(content)], re.S):
                anchor, body = match.groups()
                header = re.search(r'<h4 class="code-header">(.*?)</h4>', body, re.S)
                out.append((anchor, text_of(header.group(1)) if header else "", None, doc_after(content, match.end())))
        for match in re.finditer(r'<section id="(variant\.[^"]+)" class="variant"[^>]*>(.*?)</section>', content, re.S):
            anchor, body = match.groups()
            out.append((anchor, text_of(re.sub(r'<a href="#[^"]*" class="anchor">§</a>', "", body)), None, doc_after(content, match.end())))
        for match in re.finditer(r'<span id="(structfield\.[^"]+)" class="structfield[^"]*">(.*?)</span>', content, re.S):
            anchor, body = match.groups()
            out.append((anchor, text_of(re.sub(r'<a href="#[^"]*" class="anchor field">§</a>', "", body)), None, doc_after(content, match.end())))
        return out, len(impl_types) > 1


def doc_after(content, pos):
    """The first paragraph of the docblock that follows a member's header, if any."""
    rest = content[pos : pos + 4000]
    match = re.match(r'(?:</summary>)?<div class="docblock"><p>(.*?)</p>', rest, re.S)
    return text_of(match.group(1)) if match else ""


def self_type(header):
    """`impl<T: Send> Mutex<T>` -> `Mutex<T>`."""
    rest = header[4:].strip() if header.startswith("impl") else header
    if rest.startswith("<"):
        depth = 0
        for index, char in enumerate(rest):
            depth += {"<": 1, ">": -1}.get(char, 0)
            if depth == 0:
                rest = rest[index + 1 :].strip()
                break
    return rest.split(" where ", 1)[0].strip()


def normalize(path):
    parts = []
    for part in path.split("/"):
        if part == "..":
            parts.pop()
        elif part not in ("", "."):
            parts.append(part)
    return "/".join(parts)


MEMBER_KINDS = {
    "method": "method",
    "tymethod": "method",
    "associatedconstant": "const",
    "associatedtype": "type",
    "variant": "variant",
    "structfield": "field",
}


def api_entries(doc):
    """The API records: every item and member rustdoc documents, then source-only ones."""
    source = source_modules()
    entries = []
    for mod_path, module in sorted(source.items()):
        doc_names = doc.modules.get(mod_path)
        linux = {name for name, item in module.items.items() if "linux" in item.platforms}
        if "linux" not in module.platforms:
            continue
        if doc_names is None:
            fail(f"the source has a public module nuclei::{mod_path} that rustdoc does not document")
        missing, extra = linux - set(doc_names), set(doc_names) - linux
        if missing or extra:
            fail(
                f"nuclei::{mod_path or '(root)'}: the source and rustdoc disagree on its public names; "
                f"only in the source: {sorted(missing)}, only in rustdoc: {sorted(extra)}"
            )

    for target, info in sorted(doc.items.items()):
        if "kind" not in info:
            fail(f"a re-export points at {target}, which no module documents")
        path = info["path"]
        entry = {"n": path.rsplit("::", 1)[-1], "p": path, "k": info["kind"], "u": target, "s": info["summary"]}
        aliases = sorted(a for a in info["aliases"] if a != path)
        if aliases:
            entry["a"] = aliases
        entries.append(entry)
        if info["kind"] in ("mod", "fn", "const", "static", "macro", "attr", "derive", "type"):
            continue
        members, many_impls = doc.members(target)
        seen = set()
        for anchor, signature, impl_type, summary in members:
            if anchor in seen:
                continue
            seen.add(anchor)
            kind, name = anchor.split(".", 1)
            member = {
                "n": re.sub(r"-\d+$", "", name),
                "p": f"{path}::{re.sub(r'-[0-9]+$', '', name)}",
                "k": MEMBER_KINDS[kind],
                "u": f"{target}#{anchor}",
                "s": summary,
            }
            if signature and kind in ("method", "tymethod", "associatedconstant"):
                member["g"] = re.sub(r"^pub ", "", signature)
            if many_impls and impl_type:
                member["i"] = impl_type
            entries.append(member)

    # Public only on a platform docs.rs does not build: from the source, with that platform.
    for mod_path, module in sorted(source.items()):
        for name, item in sorted(module.items.items()):
            if "linux" in item.platforms or item.keyword == "use":
                continue
            parent = mod_path
            while parent not in doc.modules:
                parent = parent.rsplit("::", 1)[0] if "::" in parent else ""
            page = "index.html" if parent == "" else parent.replace("::", "/") + "/index.html"
            path = "nuclei::" + (mod_path + "::" if mod_path else "") + name
            platforms = ", ".join(PLATFORM_NAMES[p] for p in sorted(item.platforms))
            kind = {"mod": "mod", "struct": "struct", "enum": "enum", "trait": "trait", "fn": "fn", "type": "type", "const": "const", "static": "static"}[item.keyword]
            entries.append({"n": name, "p": path, "k": kind, "u": page, "s": item.doc, "o": platforms})
            for method, summary, here in item.methods:
                entries.append({"n": method, "p": f"{path}::{method}", "k": "method", "u": page, "s": summary, "o": ", ".join(PLATFORM_NAMES[p] for p in sorted(here))})
    return entries


# HTML: the Learn chapters and the landing page. ----------------------------------------------

VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"}


class Node:
    def __init__(self, tag, attrs, parent):
        self.tag, self.attrs, self.parent, self.children = tag, dict(attrs), parent, []

    def classes(self):
        return self.attrs.get("class", "").split()

    def find_all(self, test):
        for child in self.children:
            if isinstance(child, Node):
                if test(child):
                    yield child
                yield from child.find_all(test)

    def find(self, test):
        return next(self.find_all(test), None)


class Tree(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.root = self.node = Node("#root", {}, None)
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        node = Node(tag, attrs, self.node)
        self.node.children.append(node)
        if tag not in VOID:
            self.node = node

    def handle_endtag(self, tag):
        node = self.node
        while node is not None and node.tag != tag:
            node = node.parent
        if node is not None and node.parent is not None:
            self.node = node.parent

    def handle_data(self, data):
        self.node.children.append(data)


SKIP_CLASSES = {"visually-hidden", "copy", "snippet__label", "sidebar__num"}


def text(node):
    """The readable text of a node: no copy buttons, no status regions, no file labels."""
    out = []

    def walk(item, in_code):
        if isinstance(item, str):
            out.append(item)
            return
        if item.tag in ("button", "script", "style", "svg") or SKIP_CLASSES & set(item.classes()):
            return
        block = item.tag in ("p", "li", "pre", "tr", "td", "th", "dt", "dd", "h1", "h2", "h3", "div", "figure", "table", "br")
        in_code = in_code or item.tag in ("pre", "code")
        if block:
            out.append(" ")
        children = item.children
        for index, child in enumerate(children):
            walk(child, in_code)
            # Inline elements set side by side with no space between (the facts under the
            # headline) read as separate words; code keeps its tokens together.
            if not in_code and isinstance(child, Node) and index + 1 < len(children) and isinstance(children[index + 1], Node):
                out.append(" ")
        if block:
            out.append(" ")

    walk(node, False)
    return squash("".join(out))


def heading_sections(container):
    """Splits a container at its h2 and h3 headings: (heading or None, [nodes after it])."""
    sections, current = [], (None, [])
    for child in container.children:
        if isinstance(child, Node) and child.tag in ("h2", "h3"):
            sections.append(current)
            current = (child, [])
        else:
            current[1].append(child)
    sections.append(current)
    return sections


def joined_text(nodes):
    return squash(" ".join(text(n) if isinstance(n, Node) else n for n in nodes))


def learn_entries():
    entries = []
    for page in sorted((SITE / "learn").glob("*.html")):
        tree = Tree(page.read_text()).root
        url = "learn/" + ("" if page.name == "index.html" else page.name)
        head = tree.find(lambda n: n.tag == "header" and "chapter-head" in n.classes())
        article = tree.find(lambda n: n.tag == "article" and "prose" in n.classes())
        if head is None or article is None:
            fail(f"{page.name} has no chapter header or article")
        title = text(head.find(lambda n: n.tag == "h1"))
        step = head.find(lambda n: "chapter-head__step" in n.classes())
        number = re.search(r"\d+", text(step)).group() if step else ""
        lede = head.find(lambda n: "chapter-head__lede" in n.classes())
        numbers = {}
        toc = tree.find(lambda n: n.tag == "nav" and "toc" in n.classes())
        for link in toc.find_all(lambda n: n.tag == "a") if toc else []:
            num = link.find(lambda n: "sidebar__num" in n.classes())
            numbers[link.attrs["href"].split("#", 1)[1]] = text_of_children(num)
        # The chapter's short name, as the sidebar gives it (Migrate, not its full heading).
        current = tree.find(lambda n: n.tag == "details" and "is-current" in n.classes())
        name = text(current.find(lambda n: n.tag == "summary")) if current else title
        chapter = f"{int(number):02d} {name}" if number else title
        sections = heading_sections(article)
        intro = joined_text(sections[0][1])
        entries.append({"t": title, "c": "Learn" if not number else f"Chapter {int(number)}", "n": f"{int(number):02d}" if number else "", "u": url, "b": squash((text(lede) if lede else "") + " " + intro), "l": 1})
        parent = None
        for heading, nodes in sections[1:]:
            ident = heading.attrs.get("id")
            if not ident:
                fail(f"{page.name}: the heading {text(heading)!r} has no id to link to")
            if heading.tag == "h2":
                parent = ident
            entries.append({
                "t": text(heading),
                "c": chapter,
                "n": numbers.get(ident if heading.tag == "h2" else parent, ""),
                "u": f"{url}#{ident}",
                "b": joined_text(nodes),
                "l": 2 if heading.tag == "h2" else 3,
            })
    return entries


def text_of_children(node):
    return squash("".join(c for c in node.children if isinstance(c, str))) if node else ""


def site_entries():
    """The landing page: each section, and each card, point or note in it."""
    tree = Tree((SITE / "index.html").read_text()).root
    main = tree.find(lambda n: n.tag == "main")
    entries = []
    for section in main.find_all(lambda n: n.tag == "section" and n.parent is main):
        ident = section.attrs.get("id")
        heading = section.find(lambda n: n.tag in ("h1", "h2"))
        if heading is None:
            fail("a landing page section has no heading")
        hero = "hero" in section.classes()
        if not ident and not hero:
            fail(f"the landing page section {text(heading)!r} has no id to link to")
        url = "#top" if hero else f"#{ident}"
        entries.append({"t": text(heading), "u": url, "b": text(section)})
        for card in section.find_all(lambda n: n.tag in ("li", "div") and set(n.classes()) & {"card", "kit__item", "check", "point", "fact", "band__item"} or (n.tag == "div" and n.parent is not None and "notes" in n.parent.classes())):
            title = card.find(lambda n: n.tag in ("h3", "dt") or "band__name" in n.classes())
            if title is None:
                continue
            entries.append({"t": text(title), "c": text(heading), "u": url, "b": text(card)})
    return entries


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--doc", type=Path, help="rustdoc output to use instead of running cargo doc")
    args = parser.parse_args()
    doc = DocCrate(args.doc if args.doc else run_rustdoc())
    index = {"learn": learn_entries(), "api": api_entries(doc), "site": site_entries()}
    OUT.write_text(json.dumps(index, ensure_ascii=False, separators=(",", ":")) + "\n")

    api = index["api"]
    items = [e for e in api if e["k"] not in MEMBER_KINDS.values() or e["k"] in ("const", "type") and "#" not in e["u"]]
    learn_text = " ".join(e["t"] + " " + e["b"] for e in index["learn"])
    unnamed = sorted({e["p"] for e in items if e["k"] != "mod" and not re.search(rf"\b{re.escape(e['n'])}\b", learn_text)})
    sections = sum(1 for e in index["learn"] if e["l"] > 1)
    chapters = sum(1 for e in index["learn"] if e["l"] == 1)
    print(f"{OUT.relative_to(ROOT)}: {chapters} Learn pages, {sections} sections; {len(items)} API items and "
          f"{len(api) - len(items)} members; {len(index['site'])} landing page entries; {OUT.stat().st_size} bytes")
    if unnamed:
        print(f"public items the Learn never names ({len(unnamed)}): " + ", ".join(unnamed))


if __name__ == "__main__":
    main()
