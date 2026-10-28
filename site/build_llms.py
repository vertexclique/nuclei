"""Builds the site's files for language models, from the commit the pages are built from.

- site/llms.txt: what Nuclei is, how to add it, and a link to every Learn chapter and to the API
  reference as Markdown, after the llms.txt convention (https://llmstxt.org).
- site/learn/*.md: the Learn index and each chapter as Markdown, every program in full.
- site/api.md: the whole API reference as Markdown: every public item, its definition, its
  documentation, and each method, constant, variant and field with its signature and its
  documentation, read from the rustdoc build_search.py publishes to site/api/.
- site/llms-full.txt: all of it in one file: the Learn index, the chapters in order, the API
  reference and the changelog.

None is committed: site/build.py writes them after the pages and the reference, and the Pages
workflow deploys them with the site on every push to master, so they describe the commit the site
was built from and never lag it. The build stops if a chapter, a public item, or a file llms.txt
links to would be missing.
"""

import re
from urllib.parse import urljoin

import build_pages
from build_search import ROOT, SITE, Node, Tree, fail, squash

BASE = build_pages.BASE
LEARN = SITE / "learn"
API_MD = SITE / "api.md"
LLMS = SITE / "llms.txt"
LLMS_FULL = SITE / "llms-full.txt"

# Page furniture that is no part of the text: copy buttons and their status, rustdoc's source
# links, section anchors, the marks of notable traits and untested examples, folded summaries.
SKIP_CLASSES = {"visually-hidden", "copy", "src", "rightside", "anchor", "doc-anchor", "tooltip",
                "hideme", "main-heading", "sidebar__num"}
SKIP_TAGS = {"button", "script", "style", "svg", "input", "noscript", "wbr"}
BLOCKS = {"p", "div", "section", "article", "figure", "figcaption", "details", "summary", "pre", "ul",
          "ol", "li", "table", "thead", "tbody", "tr", "td", "th", "blockquote", "hr", "dl", "dt",
          "dd", "nav", "header", "footer", "main", "h1", "h2", "h3", "h4", "h5", "h6"}
# rustdoc's member sections: the code header of each is the member's signature.
MEMBER_SECTIONS = {"method", "associatedconstant", "associatedtype", "variant", "tymethod"}
# rustdoc's lists of trait impls, of which only the impl lines are kept (their members repeat the
# trait's own documentation); the blanket ones every type has are left out.
IMPL_LISTS = {"trait-implementations-list", "synthetic-implementations-list", "implementors-list",
              "synthetic-implementors-list"}
LEFT_OUT = {"blanket-implementations", "blanket-implementations-list"}


def skipped(node):
    return node.tag in SKIP_TAGS or bool(SKIP_CLASSES & set(node.classes()))


def raw_text(node):
    """The text of a node as written, whitespace kept (the text of a code block)."""
    if isinstance(node, str):
        return node
    if skipped(node):
        return ""
    return "".join(raw_text(c) for c in node.children)


def code_span(text):
    text = squash(text)
    if not text:
        return ""
    ticks = "`" * (max((len(r) for r in re.findall(r"`+", text)), default=0) + 1)
    pad = " " if text.startswith("`") or text.endswith("`") else ""
    return f"{ticks}{pad}{text}{pad}{ticks}"


def fence(lang, text):
    text = text.strip("\n")
    ticks = "`" * max(3, max((len(r) for r in re.findall(r"`+", text)), default=0) + 1)
    return f"{ticks}{lang}\n{text}\n{ticks}"


def tidy(text):
    text = re.sub(r"[ \t]+\n", "\n", text)
    return re.sub(r"\n{3,}", "\n\n", text).strip() + "\n"


def emphasis(mark, text):
    """`text` between `mark`s, with the spaces at its ends kept outside them."""
    core = text.strip()
    if not core:
        return text
    return (" " if text[:1].isspace() else "") + mark + core + mark + (" " if text[-1:].isspace() else "")


class Markdown:
    """HTML to Markdown for the site's pages and rustdoc's: `base` is the address relative links
    resolve against, `levels` is added to every heading's level, and `demote` sets headings as
    bold lines instead (the headings inside a member's documentation)."""

    def __init__(self, base, levels=0, demote=False):
        self.base, self.levels, self.demote = base, levels, demote

    def __call__(self, node):
        return self.blocks(node.children)

    def blocks(self, children):
        out, run = [], []

        def end_run():
            line = squash("".join(run))
            if line:
                out.append(line)
            run.clear()

        for child in children:
            if isinstance(child, str):
                run.append(child)
            elif skipped(child):
                continue
            elif child.tag in BLOCKS:
                end_run()
                block = self.block(child)
                if block.strip():
                    out.append(block)
            else:
                run.append(self.inline(child))
        end_run()
        return "\n\n".join(out)

    def block(self, node):
        tag, classes = node.tag, set(node.classes())
        if tag == "p":
            return self.inline_children(node)
        if re.fullmatch(r"h[1-6]", tag):
            text = self.inline_children(node)
            if self.demote:
                return f"**{text}**"
            return "#" * min(6, int(tag[1]) + self.levels) + " " + text
        if tag == "pre":
            return fence(self.lang(node), raw_text(node))
        if tag in ("ul", "ol"):
            return self.list(node)
        if tag == "table":
            return self.table(node)
        if tag == "blockquote":
            return quote(self.blocks(node.children))
        if tag == "hr":
            return "---"
        if tag == "dl":
            return self.definitions(node)
        if tag == "figure" and "snippet" in classes:
            return self.snippet(node)
        if tag == "div" and "note" in classes:
            title = node.find(lambda n: "note__title" in n.classes())
            body = self.blocks([c for c in node.children if c is not title])
            return quote((f"**{self.inline_children(title)}**\n\n" if title else "") + body)
        return self.blocks(node.children)

    def inline(self, node):
        if isinstance(node, str):
            return node
        if skipped(node):
            return ""
        tag = node.tag
        if tag in ("code", "kbd", "samp"):
            return code_span(raw_text(node))
        if tag == "a":
            text = squash(self.inline_children(node, keep=True))
            href = node.attrs.get("href")
            if not href or not text or href == "#":
                return text
            return f"[{text}]({urljoin(self.base, href)})"
        if tag in ("strong", "b"):
            return emphasis("**", self.inline_children(node, keep=True))
        if tag in ("em", "i"):
            return emphasis("*", self.inline_children(node, keep=True))
        if tag == "br":
            return " "
        if tag == "img":
            return node.attrs.get("alt", "")
        return self.inline_children(node, keep=True)

    def inline_children(self, node, keep=False):
        text = "".join(self.inline(c) for c in node.children)
        return re.sub(r"\s+", " ", text) if keep else squash(text)

    def lang(self, pre):
        # Learn code panels name their language; rustdoc marks Rust blocks with the class rust
        # and others with language-<name>.
        holder = pre.parent if pre.parent is not None and "data-lang" in pre.parent.attrs else pre
        lang = holder.attrs.get("data-lang")
        if lang:
            return "" if lang == "text" else lang
        for cls in pre.classes():
            if cls == "rust":
                return "rust"
            if cls.startswith("language-"):
                return cls[len("language-"):]
        return ""

    def snippet(self, figure):
        label = figure.find(lambda n: n.tag == "figcaption")
        pre = figure.find(lambda n: n.tag == "pre")
        if pre is None:
            fail("a Learn code panel holds no code")
        name = squash(raw_text(label)) if label else ""
        lead = "Output:" if name == "output" else (code_span(name) + ":" if name else "")
        return (lead + "\n\n" if lead else "") + fence(self.lang(pre), raw_text(pre))

    def list(self, node):
        ordered, number, items = node.tag == "ol", int(node.attrs.get("start", 1)), []
        for item in (c for c in node.children if isinstance(c, Node) and c.tag == "li"):
            marker = f"{number}. " if ordered else "- "
            number += 1
            lines = self.blocks(item.children).split("\n")
            items.append(marker + lines[0] + "".join("\n" + (" " * len(marker) + line if line else "") for line in lines[1:]))
        return "\n".join(items)

    def table(self, node):
        rows = []
        for row in node.find_all(lambda n: n.tag == "tr"):
            cells = [c for c in row.children if isinstance(c, Node) and c.tag in ("td", "th")]
            rows.append([self.inline_children(c).replace("|", "\\|") for c in cells])
        if not rows:
            return ""
        width = max(len(r) for r in rows)
        rows = [r + [""] * (width - len(r)) for r in rows]
        lines = ["| " + " | ".join(rows[0]) + " |", "|" + " --- |" * width]
        return "\n".join(lines + ["| " + " | ".join(r) + " |" for r in rows[1:]])

    def definitions(self, node):
        out, term = [], None
        for child in (c for c in node.children if isinstance(c, Node)):
            if child.tag == "dt":
                if term is not None:
                    out.append(f"- {term}")
                term = self.inline_children(child)
            elif child.tag == "dd":
                out.append(f"- {term}: {self.inline_children(child)}" if term else f"- {self.inline_children(child)}")
                term = None
        if term is not None:
            out.append(f"- {term}")
        return "\n".join(out)


def quote(text):
    return "\n".join("> " + line if line else ">" for line in text.split("\n"))


def header_text(node):
    """A rustdoc code header on one line: a signature, an impl line, a variant."""
    if node is None:
        return ""
    for where in node.find_all(lambda n: "where" in n.classes()):
        where.children.insert(0, " ")
    return squash(raw_text(node)).rstrip(",")


# The Learn chapters. --------------------------------------------------------------------------

def chapter_markdown(slug):
    """A built Learn page as Markdown: its title, lede, address and prose."""
    page = LEARN / ("index.html" if slug == "index" else f"{slug}.html")
    tree = Tree(page.read_text()).root
    head = tree.find(lambda n: n.tag == "header" and "chapter-head" in n.classes())
    main = tree.find(lambda n: n.tag == "main" and "learn-main" in n.classes())
    if head is None or main is None:
        fail(f"{page.relative_to(ROOT)} has no chapter header or main column")
    url = f"{BASE}learn/" + ("" if slug == "index" else f"{slug}.html")
    md = Markdown(url)
    title = md.inline_children(head.find(lambda n: n.tag == "h1"))
    lede = head.find(lambda n: "chapter-head__lede" in n.classes())
    step = head.find(lambda n: "chapter-head__step" in n.classes())
    parts = [f"# {title}"]
    if lede is not None:
        parts.append(md.inline_children(lede))
    parts.append(((md.inline_children(step) + ". ") if step else "") + f"The page: {url}")
    # The chapter's prose, and on the index the cards of the chapters under their parts.
    for child in main.children:
        if isinstance(child, Node) and child is not head and not skipped(child) and child.tag != "nav":
            parts.append(md.block(child))
    return tidy("\n\n".join(p for p in parts if p.strip()))


# The API reference. ---------------------------------------------------------------------------

class Reference:
    """The rustdoc of the crate as Markdown: each module with its documentation and its list of
    items, then each of its items in full."""

    def __init__(self, doc, api):
        self.doc, self.api = doc, api

    def page_url(self, target):
        return f"{BASE}api/nuclei/{target}"

    def item(self, target, level):
        """The Markdown of one rustdoc page under a heading of `level`."""
        tree = Tree(self.doc.page(target)).root
        main = tree.find(lambda n: n.attrs.get("id") == "main-content")
        if main is None:
            fail(f"rustdoc page {target} has no main content")
        docs = Markdown(self.page_url(target), demote=True)
        out = []

        def walk(node):
            for child in node.children:
                if not isinstance(child, Node) or skipped(child):
                    continue
                ident, classes = child.attrs.get("id", ""), set(child.classes())
                if ident in LEFT_OUT:
                    continue
                if child.tag == "pre" and "item-decl" in classes:
                    out.append(fence("rust", raw_text(child)))
                elif child.tag == "h2":
                    out.append("#" * min(6, level + 1) + " " + squash(raw_text(child)))
                elif ident in IMPL_LISTS:
                    lines = [f"- {code_span(header_text(s.find(lambda n: 'code-header' in n.classes())))}"
                             for s in child.find_all(lambda n: n.tag == "section" and "impl" in n.classes())]
                    out.append("\n".join(lines))
                elif child.tag == "section" and "impl" in classes:
                    out.append(code_span(header_text(child.find(lambda n: "code-header" in n.classes()))))
                elif child.tag == "section" and classes & MEMBER_SECTIONS:
                    out.append(fence("rust", header_text(child.find(lambda n: "code-header" in n.classes()))))
                elif child.tag == "span" and "structfield" in classes:
                    out.append(code_span(raw_text(child)))
                elif classes & {"docblock", "item-info"} or child.tag in ("p", "ul", "ol", "table", "dl", "pre"):
                    out.append(docs.block(child) if child.tag != "div" else docs(child))
                else:
                    walk(child)

        walk(main)
        return "\n\n".join(p for p in out if p.strip())

    def markdown(self, version, commit):
        items = self.doc.items
        written = set()
        parts = [f"# Nuclei {version}: API reference",
                 f"Every public item of the crate `nuclei` with its definition and documentation, and every method, "
                 f"associated constant, variant and field with its signature and documentation; for traits, their "
                 f"implementors; for types, the traits they implement. Read from the rustdoc of commit {commit} "
                 f"(x86_64-unknown-linux-gnu, default features), which is also served as HTML at {BASE}api/nuclei/."]
        for mod_path in sorted(self.doc.modules):
            target = "index.html" if mod_path == "" else mod_path.replace("::", "/") + "/index.html"
            name = "nuclei" + (f"::{mod_path}" if mod_path else "")
            parts.append(f"## {'Crate' if mod_path == '' else 'Module'} {code_span(name)}\n\n{self.page_url(target)}\n\n"
                         + self.item(target, 2))
            for item_name, (kind, item_target) in self.doc.modules[mod_path].items():
                info = items.get(item_target, {})
                if kind in ("use", "mod") or info.get("module") != mod_path or item_target in written:
                    continue
                written.add(item_target)
                aliases = sorted(a for a in info.get("aliases", ()) if a != info["path"])
                also = ("\n\nAlso exported as " + ", ".join(code_span(a) for a in aliases) + ".") if aliases else ""
                parts.append(f"### {info['kind']} {code_span(info['path'])}\n\n{self.page_url(item_target)}{also}\n\n"
                             + self.item(item_target, 3))
        missing = sorted(info["path"] for t, info in items.items() if info.get("kind") not in (None, "mod") and t not in written)
        if missing:
            fail(f"the API reference would leave out {len(missing)} public items: {', '.join(missing)}")
        # Public on a platform the reference is not built for, from the source.
        others = [e for e in self.api if "o" in e]
        if others:
            lines = [f"- {e['k']} {code_span(e['p'])} ({e['o']}){': ' + e['s'] if e['s'] else ''}" for e in others]
            parts.append("## Items on other platforms\n\nPublic only on the platforms named, so not in the reference "
                         "above, which is built for Linux:\n\n" + "\n".join(lines))
        return tidy("\n\n".join(parts)), len(written)


# The files. -----------------------------------------------------------------------------------

def summary_line():
    landing = (SITE / "src" / "index.html").read_text()
    match = re.search(r'<meta name="description" content="Nuclei: ([^"]+)"', landing)
    if match is None:
        fail("site/src/index.html has no description to summarize Nuclei with")
    text = match.group(1)
    return text[0].upper() + text[1:]


def intro(values, commit):
    version, rust = values["NUCLEI_VERSION"], values["RUST_VERSION"]
    return (f"> {summary_line()}\n\n"
            f"Nuclei {version} needs Rust {rust} or newer. Add it to Cargo.toml:\n\n"
            + fence("toml", f'[dependencies]\nnuclei = "{version}"') + "\n\n"
            f"These files are built from commit {commit} of github.com/vertexclique/nuclei with the site, on every "
            f"push to master, so they describe the code as it is there. The Learn chapters teach the crate from the "
            f"first program to production, with a program that compiles and runs for every idea; the API reference "
            f"holds every public item.")


def write(doc, api):
    values, _ = build_pages.source_values()
    commit = values["SOURCE"].rstrip("/").rsplit("/", 1)[-1]
    chapters = [("index", "Learn Nuclei", "Where to start, and the chapters in order.")]
    chapters += [(slug, name, line) for _, slug, name, line in build_pages.CHAPTERS]

    learn = {}
    for slug, _, _ in chapters:
        learn[slug] = chapter_markdown(slug)
        (LEARN / f"{slug}.md").write_text(learn[slug])

    reference, count = Reference(doc, api).markdown(values["NUCLEI_VERSION"], commit)
    API_MD.write_text(reference)

    links = "\n".join(f"- [{name}]({BASE}learn/{slug}.md): {line}" for slug, name, line in chapters)
    changelog_url = f"https://raw.githubusercontent.com/vertexclique/nuclei/{commit}/CHANGELOG.md"
    LLMS.write_text(tidy(
        f"# Nuclei\n\n{intro(values, commit)}\n\n"
        f"## Learn\n\n{links}\n\n"
        f"## API\n\n- [API reference]({BASE}api.md): every public item and member, with its signature and documentation\n"
        f"- [rustdoc]({BASE}api/nuclei/): the same reference as HTML\n\n"
        f"## Optional\n\n- [Everything in one file]({BASE}llms-full.txt): the Learn chapters, the API reference and the changelog\n"
        f"- [Changelog]({changelog_url}): what changed in each release, and how to move from 0.4"))

    changelog = (ROOT / "CHANGELOG.md").read_text()
    LLMS_FULL.write_text(tidy("\n\n".join(
        [f"# Nuclei: everything\n\n{intro(values, commit)}\n\nWhat follows: the Learn index, the {len(chapters) - 1} "
         f"Learn chapters in order, the API reference, and the changelog."]
        + [learn[slug] for slug, _, _ in chapters] + [reference, changelog])))

    # Every address llms.txt names is a file this build wrote.
    for url in re.findall(r"\]\((https://nuclei\.rs/[^)]+)\)", LLMS.read_text()):
        path = SITE / url[len(BASE):]
        if path.is_dir():
            path = path / "index.html"
        if not path.exists():
            fail(f"llms.txt links to {url}, which the build did not write")
    print(f"{LLMS.relative_to(ROOT)}: {len(chapters)} Learn pages as Markdown and the API reference "
          f"({count} items, {API_MD.stat().st_size} bytes); {LLMS_FULL.relative_to(ROOT)}: {LLMS_FULL.stat().st_size} bytes")
