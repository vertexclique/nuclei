"""Builds the site's pages from their sources: site/index.html from site/src/index.html, and every
Learn page in site/learn/ from site/src/learn/. site/build.py runs it; nothing it writes is
committed.

The sources:

- site/src/index.html is the landing page as it is served, but for the markers below.
- site/src/learn/<slug>.html is one Learn page: two header lines, then its prose.

      <!--title Time-->
      <!--lede One line under the title.-->

  Which pages there are, their order, their parts and the line on each card is CHAPTERS below.
  The bar, the chapter list, the sidebar of sections, the pager and the footer are written here,
  so they are the same on every page.
- site/examples/ is a crate of its own: every program a page shows is a file of it,
  src/bin/<name>.rs (or a test file, tests/<name>.rs), shown byte for byte. `site/build.py
  --examples` builds, lints and runs each one on every Linux backend, and the Pages workflow does
  so before it deploys.

The markers:

    <!--example NAME-->              the program site/examples/src/bin/NAME.rs, labelled src/main.rs
    <!--example NAME LABEL-->        the same with another label, or a test file tests/NAME.rs
    <!--example-code NAME-->         only its highlighted code, for a panel written in the page
    <!--output NAME-->TEXT<!--/output-->
                                     what NAME prints, shown as its output; the example run checks
                                     the program prints exactly that
    <!--code LANG LABEL-->TEXT<!--/code-->
                                     code written in the page: rust, toml, sh or text; LABEL is
                                     optional
    <!--features-->LINES<!--/features-->
                                     the crate's features as a table: one `name: what it gives`
                                     line per feature of Cargo.toml, which must name each one
    href="api:nuclei::sync::Mutex"   the API reference's page of that item (or member,
                                     nuclei::Task::detach); a path the reference lacks stops the build
    <!--api-table                    a table of items, each linked and with the summary its
    Name | What it is                documentation opens with (defaults stated once, in the
    nuclei::Builder::worker_threads  source); the first line names the two columns
    -->
    @@NAME@@                         a value of the source: NUCLEI_VERSION, NUCLEI_MINOR,
                                     RUST_VERSION, CI_KERNELS (the Linux series the kernel matrix
                                     runs), SOURCE (the repository at the commit built)

Every link between pages is checked: a link to a page or an anchor that does not exist stops the
build, as does an example on no page or a marker naming none.
"""

import html
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
SRC = SITE / "src"
EXAMPLES = SITE / "examples"
BASE = "https://nuclei.rs/"

# The Learn path: (part, slug, sidebar name, the line on its card and in its description).
CHAPTERS = [
    ("Start", "setup", "Setup", "Add Nuclei, run a first program, pick features, see which backend runs."),
    ("Start", "proactive", "How it works", "Shards, the proactor and its buffers: the few ideas the rest builds on."),
    ("Core", "runtime", "Runtime", "Enter the runtime: the attribute, block_on, a runtime of your own."),
    ("Core", "tasks", "Tasks", "Spawn, join, cancel and name tasks; local, blocking and task-local work."),
    ("Core", "select", "Select and join", "Wait on several futures in one task: join!, try_join! and select!."),
    ("IO", "net", "Networking", "TCP, UDP and Unix sockets, names, socket options, halves and pipes."),
    ("IO", "io", "IO utilities", "Read and write helpers, copies, buffers, in-memory pipes and the standard streams."),
    ("IO", "readiness", "Readiness", "Readiness waits, the try_ calls, and system calls of your own."),
    ("IO", "fs", "Files", "Files, whole-file calls, metadata and directories."),
    ("Building blocks", "time", "Time", "Sleeps, timeouts, intervals, and a clock tests can stop."),
    ("Building blocks", "sync", "Sync", "Locks, semaphores, notifications, once-cells and lock-free structures."),
    ("Building blocks", "channels", "Channels", "mpsc, oneshot, broadcast and watch channels, and cancellation tokens."),
    ("Building blocks", "processes", "Processes", "Child processes with async pipes, and signals as streams."),
    ("Ship it", "production", "Production", "Every runtime setting, hooks, counters, shutdown, tests and allocation."),
    ("Ship it", "backends", "Backends", "io_uring, epoll, kqueue and IOCP: how one is picked and what differs."),
    ("Ship it", "errors", "Errors", "The messages you may meet, what each means, and the fix."),
    ("Ship it", "migrate", "Migrate", "Coming from a readiness-based runtime: the map and the pitfalls."),
]

# Pages that moved: the old address sends the reader to the new one.
MOVED = {"handles": "net"}


def fail(message):
    sys.exit(f"build_pages: {message}")


# Highlighting. -------------------------------------------------------------------------------

KEYWORDS = {
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while",
}

RUST = re.compile(r"""
  (?P<cm>//[^\n]*)
 |(?P<at>\#!?\[[^\]\n]*\])
 |(?P<st>b?r\#*"(?:.|\n)*?"\#*|b?"(?:\\.|[^"\\])*"|b?'(?:\\.|[^'\\])')
 |(?P<lt>'[A-Za-z_][A-Za-z0-9_]*)
 |(?P<nu>\b\d[\d_]*(?:\.\d+)?(?:[iu](?:8|16|32|64|128|size)|f32|f64)?\b)
 |(?P<id>[A-Za-z_][A-Za-z0-9_]*!?)
 |(?P<pu>::|->|=>|\.\.=?|[-+*/%^!&|=<>?@.,;:(){}\[\]#'])
 |(?P<ws>\s+)
 |(?P<other>.)
""", re.X | re.S)


def rust(src):
    out = []
    for match in RUST.finditer(src):
        kind, text = match.lastgroup, match.group()
        if kind == "id":
            after = src[match.end():match.end() + 3]
            if text.endswith("!"):
                kind = "ma"
            elif text in KEYWORDS:
                kind = "kw"
            elif text == "_":
                kind = "pu"
            elif text[0].isupper():
                kind = "ty"
            elif after.startswith("::"):
                kind = "md"
            elif after.startswith("(") or after.startswith("::<"):
                kind = "fn"
            else:
                kind = None
        elif kind == "lt":
            kind = "ty"
        elif kind in ("ws", "other"):
            kind = None
        out.append((kind, text))
    return out


TOML = re.compile(r"""
  (?P<cm>\#[^\n]*)
 |(?P<sec>^\[[^\]\n]+\])
 |(?P<st>"(?:\\.|[^"\\])*")
 |(?P<nu>\b(?:true|false|\d[\d_.]*)\b)
 |(?P<key>[A-Za-z_][A-Za-z0-9_-]*)(?=\s*=)
 |(?P<pu>[=,{}\[\]])
 |(?P<ws>\s+)
 |(?P<other>.)
""", re.X | re.M)


def toml(src):
    out = []
    for match in TOML.finditer(src):
        kind, text = match.lastgroup, match.group()
        if kind == "sec":
            out += [("pu", "["), ("kw", text[1:-1]), ("pu", "]")]
        elif kind == "key":
            out.append(("fn", text))
        else:
            out.append((kind if kind not in ("ws", "other") else None, text))
    return out


def shell(src):
    """One command: its variables, the program, its subcommand, flags and arguments."""
    out = []
    words = src.split(" ")
    stage = 0
    for index, word in enumerate(words):
        if index:
            out.append((None, " "))
        if stage == 0 and re.fullmatch(r"[A-Z_]+=\S*", word):
            out.append(("at", word))
        elif stage == 0:
            out.append(("fn", word))
            stage = 1
        elif stage == 1 and not word.startswith("-"):
            out.append(("kw", word))
            stage = 2
        elif word.startswith("-"):
            out.append(("pu", word))
        else:
            out.append((None, word))
    return out


def render(tokens):
    merged = []
    for kind, text in tokens:
        if merged and merged[-1][0] == kind:
            merged[-1] = (kind, merged[-1][1] + text)
        else:
            merged.append((kind, text))
    return "".join(f'<span class="{k}">{html.escape(t, quote=False)}</span>' if k else html.escape(t, quote=False)
                   for k, t in merged)


HIGHLIGHT = {"rust": rust, "toml": toml, "sh": shell}

COPY = ('<button class="copy" type="button" data-copy="{id}" hidden><span class="copy__face copy__face--idle">Copy</span>'
        '<span class="copy__face copy__face--done" aria-hidden="true">Copied</span>'
        '<span class="copy__face copy__face--error" aria-hidden="true">Select it</span></button>'
        '<span class="visually-hidden" role="status" aria-live="polite"></span>')


# Values from the source. ---------------------------------------------------------------------

def source_values():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    package = manifest["package"]
    version, rust_version = package.get("version"), package.get("rust-version")
    if not version or not rust_version:
        fail("Cargo.toml gives no version or rust-version")
    kernels = re.search(r"series:\s*\[([^\]]*)\]", (ROOT / ".github/workflows/kernels.yml").read_text())
    if kernels is None:
        fail(".github/workflows/kernels.yml has no matrix of kernel series")
    series = [s.strip().strip("\"'") for s in kernels.group(1).split(",") if s.strip()]
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True)
    if commit.returncode != 0 or not commit.stdout.strip():
        fail("git names no commit to link the source at")
    return {
        "NUCLEI_VERSION": version,
        "NUCLEI_MINOR": ".".join(version.split(".")[:2]),
        "RUST_VERSION": rust_version,
        "CI_KERNELS": ", ".join(series[:-1]) + " and " + series[-1] if len(series) > 1 else series[0],
        # The repository's files as this commit has them, so a link never names a file the
        # branch it was built from lacks.
        "SOURCE": f"https://github.com/vertexclique/nuclei/blob/{commit.stdout.strip()}/",
    }, manifest["features"]


def fill_values(text, values, where):
    def value(match):
        if match.group(1) not in values:
            fail(f"{where}: no value for @@{match.group(1)}@@")
        return values[match.group(1)]

    return re.sub(r"@@([A-Z_]+)@@", value, text)


# Pages. --------------------------------------------------------------------------------------

class Page:
    """One page's expansion: its code panels are numbered from one."""

    def __init__(self, builder, where):
        self.builder, self.where, self.panels = builder, where, 0

    def panel(self, lang, code, label=""):
        code = code.strip("\n")
        caption = f'<figcaption class="snippet__label">{html.escape(label)}</figcaption>' if label else ""
        if lang == "text":
            return (f'<figure class="snippet snippet--output">{caption}<div class="code"><pre><code>'
                    f'{html.escape(code, quote=False)}</code></pre></div></figure>')
        if lang not in HIGHLIGHT:
            fail(f"{self.where}: no highlighting for {lang!r}")
        self.panels += 1
        ident = f"code-{self.panels}"
        shell_class = " code--shell" if lang == "sh" else ""
        return (f'<figure class="snippet">{caption}<div class="code code--corner{shell_class}">'
                f'<pre id="{ident}"><code>{render(HIGHLIGHT[lang](code))}</code></pre>{COPY.format(id=ident)}</div></figure>')

    def expand(self, text):
        b = self.builder

        def example(match):
            name, label = match.group(1), (match.group(2) or "").strip() or "src/main.rs"
            return self.panel("rust", b.example(name, self.where), label)

        def example_code(match):
            return render(rust(b.example(match.group(1), self.where).strip("\n")))

        def output(match):
            name, printed = match.group(1), html.unescape(match.group(2)).strip("\n")
            b.example(name, self.where)
            b.outputs[name] = printed
            return self.panel("text", printed, "output")

        def code(match):
            return self.panel(match.group(1), html.unescape(match.group(3)), (match.group(2) or "").strip())

        def features(match):
            return b.features_table(match.group(1), self.where)

        def api(match):
            return f'href="{b.api_url(match.group(1), self.where)}"'

        def api_table(match):
            return b.api_table(match.group(1), self.where)

        text = re.sub(r"<!--example ([a-z0-9_]+)( [^>]*?)?-->", example, text)
        text = re.sub(r"<!--example-code ([a-z0-9_]+)-->", example_code, text)
        text = re.sub(r"<!--output ([a-z0-9_]+)-->\n?(.*?)<!--/output-->", output, text, flags=re.S)
        text = re.sub(r"<!--code (\w+)([^>]*?)-->\n?(.*?)<!--/code-->", code, text, flags=re.S)
        text = re.sub(r"<!--features-->\n?(.*?)<!--/features-->", features, text, flags=re.S)
        text = re.sub(r"<!--api-table\n(.*?)-->", api_table, text, flags=re.S)
        text = re.sub(r'href="api:([A-Za-z0-9_:]+)"', api, text)
        if re.search(r"<!--/?(example|output|code|features|api-table)", text):
            fail(f"{self.where}: a marker is malformed or not closed")
        return fill_values(text, b.values, self.where)


class Builder:
    def __init__(self, api_urls):
        self.api = api_urls
        self.values, self.cargo_features = source_values()
        self.used = set()
        self.outputs = {}
        self.mark = None

    def prefix(self, where):
        return "../" if where.startswith("learn/") else ""

    def example(self, name, where):
        paths = [EXAMPLES / "src" / "bin" / f"{name}.rs", EXAMPLES / "tests" / f"{name}.rs"]
        path = next((p for p in paths if p.exists()), None)
        if path is None:
            fail(f"{where}: no example {name}: neither {' nor '.join(str(p.relative_to(ROOT)) for p in paths)}")
        self.used.add(name)
        return path.read_text()

    def api_url(self, path, where):
        if path not in self.api:
            fail(f"{where}: the API reference has no page for {path}")
        return f"{self.prefix(where)}api/nuclei/{self.api[path][0]}"

    def api_table(self, lines, where):
        """A table of API items, each with the summary its documentation opens with, so a
        default or a meaning is stated once, in the source."""
        lines = [line.strip() for line in lines.strip().splitlines() if line.strip()]
        heads = [h.strip() for h in lines[0].split("|")]
        if len(heads) != 2 or len(lines) < 2:
            fail(f"{where}: an api-table starts with `Name | What it is` and lists paths after")
        rows = []
        for path in lines[1:]:
            name = path.rsplit("::", 1)[-1]
            summary = self.api.get(path, ("", ""))[1]
            if not summary:
                fail(f"{where}: {path} has no summary in the API reference to show")
            rows.append(f'<tr><td data-label="{html.escape(heads[0])}"><a href="{self.api_url(path, where)}"><code>{html.escape(name)}</code></a></td>'
                        f'<td data-label="{html.escape(heads[1])}">{html.escape(summary)}</td></tr>\n')
        return (f'<table class="table">\n<thead><tr><th>{html.escape(heads[0])}</th><th>{html.escape(heads[1])}</th></tr></thead>\n'
                f"<tbody>\n{''.join(rows)}</tbody>\n</table>")

    def features_table(self, lines, where):
        described = {}
        for line in lines.strip().splitlines():
            name, sep, what = line.partition(":")
            if not sep:
                fail(f"{where}: a features line is not `name: what it gives`: {line!r}")
            described[name.strip()] = what.strip()
        defaults = set(self.cargo_features.get("default", []))
        names = [n for n in self.cargo_features if n != "default"]
        if set(names) != set(described):
            fail(f"{where}: the features table names {sorted(described)}, Cargo.toml has {sorted(names)}")
        rows = "".join(
            f'<tr><td data-label="Feature"><code>{n}</code></td><td data-label="Default">{"on" if n in defaults else "off"}</td>'
            f'<td data-label="What it gives">{described[n]}</td></tr>\n' for n in names)
        return ('<table class="table">\n<thead><tr><th>Feature</th><th>Default</th><th>What it gives</th></tr></thead>\n'
                f"<tbody>\n{rows}</tbody>\n</table>")

    # The parts of every Learn page. ----------------------------------------------------------

    def head(self, title, description, url, learn_current):
        return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{html.escape(title)}</title>
  <meta name="description" content="{html.escape(description)}">
  <link rel="canonical" href="{url}">
  <meta property="og:title" content="{html.escape(title)}">
  <meta property="og:description" content="{html.escape(description)}">
  <meta property="og:type" content="article">
  <meta property="og:url" content="{url}">
  <meta property="og:image" content="{BASE}media/social.png">
  <meta property="og:image:width" content="1200">
  <meta property="og:image:height" content="630">
  <meta property="og:image:alt" content="The Nuclei logo: a diamond inside a diamond, with eight arrows pointing out">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="theme-color" content="#05090f">
  <meta name="color-scheme" content="dark">
  <link rel="icon" href="../favicon-32.png" type="image/png" sizes="32x32">
  <link rel="icon" href="../favicon.svg" type="image/svg+xml">
  <link rel="apple-touch-icon" href="../apple-touch-icon.png">
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap">
  <link rel="stylesheet" href="../tokens.css">
  <link rel="stylesheet" href="../site.css">
  <link rel="stylesheet" href="../code.css">
  <link rel="stylesheet" href="../cards.css">
  <link rel="stylesheet" href="../learn.css">
  <link rel="stylesheet" href="../search.css">
  <link rel="stylesheet" href="../motion.css">
</head>
<body id="top">
  <header class="nav">
    <a class="nav__mark" href="../">{self.mark}Nuclei</a>
    <nav class="nav__links" aria-label="Primary">
      <a class="nav__link" href="./"{' aria-current="page"' if learn_current else ""}>Learn</a>
      <a class="nav__link nav__link--mid" href="../api/nuclei/">API</a>
      <a class="nav__link nav__link--wide" href="https://github.com/vertexclique/nuclei">Source</a>
      <button class="nav__search" type="button" data-search aria-haspopup="dialog" hidden><svg class="nav__search-icon" viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2" width="8.5" height="8.5"/><path d="M10.5 10.5 14 14"/></svg><span class="nav__search-label">Search</span><kbd class="nav__kbd" data-search-key aria-hidden="true">Ctrl K</kbd></button>
      <a class="nav__cta" href="../#install">Install</a>
    </nav>
  </header>
"""

    FOOT = """
  <footer class="foot">
    <div class="foot__meta">
      <span class="foot__mark">Nuclei</span>
      <span><a href="https://crates.io/crates/nuclei">crates.io</a> &middot; Apache-2.0 or MIT</span>
      <span class="foot__links"><a href="../">Home</a> <a href="./">Learn</a> <a href="../api/nuclei/">API</a> <a href="https://github.com/vertexclique/nuclei">Source</a> <a href="@@SOURCE@@CHANGELOG.md">Changes</a></span>
    </div>
  </footer>

  <a class="to-top" href="#top" data-to-top aria-label="Back to top" title="Back to top"><svg viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="M3.5 10 8 5.5l4.5 4.5"/></svg></a>
  <script src="../site.js"></script>
</body>
</html>
"""

    def sections(self, slug):
        """A chapter's sections: the id and text of each h2 of its source."""
        source = (SRC / "learn" / f"{slug}.html").read_text()
        return [(i, html.unescape(re.sub(r"<[^>]+>", "", t))) for i, t in re.findall(r'<h2 id="([^"]+)">(.*?)</h2>', source)]

    def section_links(self, number, slug):
        return "".join(
            f'<li><a href="{slug}.html#{ident}" title="{html.escape(text)}"><span class="sidebar__num">{number:02d}.{k:02d}</span>'
            f'<span class="sidebar__text">{html.escape(text)}</span></a></li>'
            for k, (ident, text) in enumerate(self.sections(slug), 1))

    def chapter_nav(self, current):
        """The chapters twice from one list: chips for a narrow screen, and for a wide one a sidebar of
        closed chapters under their parts (native details, one open at a time by name), the current
        page's chapter open on its numbered sections."""
        chips, side, part = [], [], None
        for number, (chapter_part, slug, name, _) in enumerate(CHAPTERS, 1):
            here = slug == current
            current_attr = ' aria-current="page"' if here else ""
            open_attr = ' open class="is-current"' if here else ""
            chips.append(f'<li><a href="{slug}.html"{current_attr}>{html.escape(name)}</a></li>')
            if chapter_part != part:
                part = chapter_part
                side.append(f'<p class="sidebar__part">{html.escape(part)}</p>')
            side.append(f'<details name="chapters"{open_attr}><summary><span class="sidebar__num">{number:02d}</span>'
                        f'{html.escape(name)}</summary><ol>{self.section_links(number, slug)}</ol></details>')
        return ('    <nav class="chapters" aria-label="Chapters">\n'
                '      <p class="chapters__title"><a href="./">Learn Nuclei</a></p>\n'
                f'      <ol>{"".join(chips)}</ol>\n    </nav>\n'
                '    <nav class="sidebar" aria-label="Chapters and sections">\n'
                '      <p class="chapters__title"><a href="./">Learn Nuclei</a></p>\n'
                f'      {"".join(side)}\n    </nav>\n')

    def read_source(self, slug):
        source = (SRC / "learn" / f"{slug}.html").read_text()
        match = re.match(r"<!--title (.*?)-->\n<!--lede (.*?)-->\n", source, re.S)
        if match is None:
            fail(f"src/learn/{slug}.html does not start with its title and lede lines")
        return match.group(1), match.group(2), source[match.end():]

    def chapter(self, number, slug, name, description):
        where = f"learn/{slug}.html"
        title, lede, body = self.read_source(slug)
        page = Page(self, where)
        body = page.expand(body)
        lede = page.expand(lede)
        pager = ""
        if number > 1:
            p = CHAPTERS[number - 2]
            pager += f'<a href="{p[1]}.html" rel="prev"><span class="visually-hidden">Previous chapter: </span><span class="pager__title">{html.escape(p[2])}</span></a>'
        if number < len(CHAPTERS):
            n = CHAPTERS[number]
            pager += f'<a href="{n[1]}.html" rel="next"><span class="visually-hidden">Next chapter: </span><span class="pager__title">{html.escape(n[2])}</span></a>'
        out = self.head(f"{re.sub('<[^>]+>', '', title)}: Learn Nuclei", description, f"{BASE}learn/{slug}.html", False)
        out += '\n  <div class="learn-wrap">\n' + self.chapter_nav(slug)
        out += ('    <main class="learn-main">\n'
                f'      <header class="chapter-head">\n        <p class="chapter-head__step ink">Chapter {number} of {len(CHAPTERS)}</p>\n'
                f'        <h1>{title}</h1>\n        <p class="chapter-head__lede">{lede}</p>\n      </header>\n'
                f'      <nav class="toc" aria-label="On this page"><p class="toc__title">In this chapter</p><ol>{self.section_links(number, slug)}</ol></nav>\n'
                f'      <article class="prose">\n{body.strip()}\n      </article>\n'
                f'      <nav class="pager" aria-label="Chapters">{pager}</nav>\n'
                '    </main>\n  </div>\n')
        return out + self.FOOT

    def learn_index(self):
        title, lede, body = self.read_source("index")
        page = Page(self, "learn/index.html")
        body = page.expand(body)
        parts = []
        for number, (part, slug, name, line) in enumerate(CHAPTERS, 1):
            if not parts or parts[-1][0] != part:
                parts.append((part, []))
            parts[-1][1].append(f'<a class="card tile" href="{slug}.html" data-reveal style="--n: {len(parts[-1][1]) % 3}">'
                                f'<span class="card__step ink">{number:02d}</span><h3>{html.escape(name)}</h3><p>{html.escape(line)}</p></a>')
        cards = []
        for part, items in parts:
            ident = "part-" + re.sub(r"[^a-z]+", "-", part.lower()).strip("-")
            cards.append(f'      <div class="learn-part"><h2 id="{ident}">{html.escape(part)}</h2>'
                         f'<div class="learn-cards">{"".join(items)}</div></div>\n')
        description = ("Learn Nuclei, the proactive async runtime for Rust: from the first line in Cargo.toml "
                       "to a server in production, with a program that compiles and runs for every idea.")
        out = self.head("Learn Nuclei", description, f"{BASE}learn/", True)
        out += '\n  <div class="learn-wrap">\n' + self.chapter_nav(None)
        out += ('    <main class="learn-main">\n'
                f'      <header class="chapter-head">\n        <h1>{title}</h1>\n        <p class="chapter-head__lede">{page.expand(lede)}</p>\n      </header>\n'
                f'      <article class="prose">\n{body.strip()}\n      </article>\n{"".join(cards)}'
                '    </main>\n  </div>\n')
        return out + self.FOOT

    def moved(self, old, new):
        url = f"{BASE}learn/{new}.html"
        return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Moved: Learn Nuclei</title>
  <link rel="canonical" href="{url}">
  <meta http-equiv="refresh" content="0; url={new}.html">
</head>
<body>
  <p>This page moved to <a href="{new}.html">{new}.html</a>.</p>
</body>
</html>
"""

    def build(self):
        landing = (SRC / "index.html").read_text()
        mark = re.search(r'<span class="nav__logo">.*?</span>', landing, re.S)
        if mark is None:
            fail("src/index.html has no header mark (art/build.py writes it)")
        self.mark = mark.group()
        pages = {"index.html": Page(self, "index.html").expand(landing)}
        pages["learn/index.html"] = self.learn_index()
        for number, (_, slug, name, line) in enumerate(CHAPTERS, 1):
            pages[f"learn/{slug}.html"] = self.chapter(number, slug, name, line)
        for old, new in MOVED.items():
            pages[f"learn/{old}.html"] = self.moved(old, new)

        sources = {p.stem for p in (SRC / "learn").glob("*.html")}
        listed = {slug for _, slug, _, _ in CHAPTERS} | {"index"}
        if sources != listed:
            fail(f"site/src/learn/ and CHAPTERS disagree: only there {sorted(sources - listed)}, only listed {sorted(listed - sources)}")
        unused = {p.stem for d in ("src/bin", "tests") for p in (EXAMPLES / d).glob("*.rs")} - self.used
        if unused:
            fail(f"examples on no page: {', '.join(sorted(unused))}")
        pages = {rel: fill_values(text, self.values, rel) for rel, text in pages.items()}
        check_links(pages)

        learn = SITE / "learn"
        learn.mkdir(exist_ok=True)
        for stale in learn.glob("*.html"):
            if f"learn/{stale.name}" not in pages:
                stale.unlink()
        for rel, text in pages.items():
            (SITE / rel).write_text(text)
        return pages


def check_links(pages):
    """Every relative link of every page reaches a file the site serves, and its anchor an id there."""
    ids = {rel: set(re.findall(r'\bid="([^"]+)"', text)) for rel, text in pages.items()}
    for rel, text in pages.items():
        base = rel.rsplit("/", 1)[0] + "/" if "/" in rel else ""
        for href in re.findall(r'\bhref="([^"]+)"', text):
            if re.match(r"[a-z]+:", href) or href.startswith("//"):
                continue
            target, _, anchor = href.partition("#")
            parts = []
            for part in (base + target).split("/"):
                if part == "..":
                    if not parts:
                        fail(f"{rel}: the link {href} leaves the site")
                    parts.pop()
                elif part not in ("", "."):
                    parts.append(part)
            path = "/".join(parts)
            if target == "":
                path = rel
            elif target.endswith("/") or path == "":
                path = f"{path}/index.html" if path else "index.html"
            if path in pages:
                if anchor and anchor not in ids[path]:
                    fail(f"{rel}: the link {href} names no id of {path}")
                continue
            file = SITE / path
            if not file.exists():
                fail(f"{rel}: the link {href} reaches no file of the site")
            if anchor and file.suffix == ".html" and f'id="{anchor}"' not in file.read_text():
                fail(f"{rel}: the link {href} names no id of {path}")


def build(api_urls):
    """Writes every page; answers the outputs the pages show for their examples, by example."""
    builder = Builder(api_urls)
    builder.build()
    return builder.outputs
