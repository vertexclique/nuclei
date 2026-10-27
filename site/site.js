// The site's one script, shared by every page. Nothing on a page depends on it: without it the
// code is plain text to select, every section is shown at rest, the footage stays on its still
// frame and the search button stays hidden. With it: copy buttons, search, reveals as sections
// scroll in, the hero footage, and the loops that run only while their section is on screen.

const still = matchMedia("(prefers-reduced-motion: reduce)");
// The site's root, from where this script is served: every page loads it from there.
const siteRoot = new URL(".", document.currentScript.src);

// Copy buttons exist only where they work. The button copies its code's text, never its markup.
for (const button of document.querySelectorAll("[data-copy]")) {
  const status = button.nextElementSibling;
  const source = document.getElementById(button.dataset.copy);
  const panel = source.closest(".code");
  let timer = 0;
  button.hidden = false;
  button.addEventListener("click", async () => {
    clearTimeout(timer);
    // Cleared and set again, so a second copy replays its confirmation.
    delete button.dataset.state;
    delete panel.dataset.copied;
    void button.offsetWidth;
    try {
      await navigator.clipboard.writeText(source.textContent);
      button.dataset.state = "copied";
      panel.dataset.copied = "";
      status.textContent = "Copied";
    } catch {
      button.dataset.state = "error";
      status.textContent = "Copy failed: select the lines and copy them";
    }
    timer = setTimeout(() => {
      delete button.dataset.state;
      delete panel.dataset.copied;
      status.textContent = "";
    }, 2500);
  });
}

// Search: Cmd+K on Apple platforms or Ctrl+K elsewhere, on every page, or the search button in
// the bar. The dialog is built on its first opening and the index (search.json, which
// build_search.py writes from the pages and the crate) is fetched then, once. A modal <dialog>
// keeps the page behind it inert; the input is a combobox whose results are a listbox, moved
// through with the arrow keys while focus stays in the input. Tab stays inside the dialog, and
// closing it gives focus back to what had it.
{
  const apple = /Mac|iPhone|iPad|iPod/.test(navigator.userAgentData?.platform ?? navigator.platform ?? "");
  // The API reference the deploy builds from the same commit as the index (build_search.py).
  const docs = new URL("api/nuclei/", siteRoot).href;
  const groups = { learn: "Learn", api: "API", site: "Home page" };
  const shown = { learn: 6, api: 8, site: 4 };
  const memberKinds = new Set(["method", "variant", "field"]);
  let dialog, input, list, status, index, loading, opener;
  let options = [];
  let active = -1;
  let expanded = null;

  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };

  // The text with every match of a term wrapped in <mark>.
  const marked = (text, terms) => {
    const low = text.toLowerCase();
    const ranges = [];
    for (const term of terms) {
      for (let at = low.indexOf(term); at >= 0; at = low.indexOf(term, at + term.length)) ranges.push([at, at + term.length]);
    }
    ranges.sort((a, b) => a[0] - b[0]);
    const out = document.createDocumentFragment();
    let pos = 0;
    for (const [start, end] of ranges) {
      if (end <= pos) continue;
      const from = Math.max(start, pos);
      out.append(text.slice(pos, from), el("mark", "", text.slice(from, end)));
      pos = end;
    }
    out.append(text.slice(pos));
    return out;
  };

  // A stretch of the text around its first match, cut at word boundaries.
  const snippet = (text, terms) => {
    const low = text.toLowerCase();
    const hits = terms.map((t) => low.indexOf(t)).filter((i) => i >= 0);
    const at = hits.length ? Math.min(...hits) : 0;
    let start = Math.max(0, at - 48);
    let end = Math.min(text.length, at + 120);
    const space = text.indexOf(" ", start);
    if (start > 0 && space >= 0 && space < at) start = space + 1;
    if (end < text.length) end = Math.max(text.lastIndexOf(" ", end), at + 1);
    return (start > 0 ? "…" : "") + text.slice(start, end).trim() + (end < text.length ? "…" : "");
  };

  // Where a term starts a word of the title: at its start, after a separator, or at a capital
  // that follows a small letter (Token in CancellationToken).
  const startsWord = (title, low, term) => {
    for (let at = low.indexOf(term); at >= 0; at = low.indexOf(term, at + 1)) {
      if (at === 0 || !/[a-z0-9]/.test(low[at - 1])) return true;
      if (/[A-Z]/.test(title[at]) && /[a-z]/.test(title[at - 1])) return true;
    }
    return false;
  };

  const score = (record, terms, phrase) => {
    let total = 0;
    for (const term of terms) {
      const head = record.low;
      let points;
      if (head === term) points = 100;
      else if (head.startsWith(term)) points = 70;
      else if (startsWord(record.title, head, term)) points = 55;
      else if (head.includes(term)) points = 40;
      else if (record.where.includes(term)) points = 22;
      else if (record.body.includes(term)) points = 8;
      else return 0;
      total += points;
    }
    if (terms.length > 1) total += record.low.includes(phrase) ? 30 : record.body.includes(phrase) ? 10 : 0;
    return total + record.weight;
  };

  // The records of the index, each with its lower-cased fields and what it links to.
  const prepare = (data) => {
    const records = [];
    for (const r of data.learn) {
      records.push({ kind: "learn", data: r, title: r.t, low: r.t.toLowerCase(), where: `${r.c} ${r.n}`.toLowerCase(),
        body: r.b.toLowerCase(), url: new URL(r.u, siteRoot).href, weight: 5 - r.l });
    }
    for (const r of data.api) {
      const member = memberKinds.has(r.k) || r.u.includes("#");
      records.push({ kind: "api", data: r, title: r.n, low: r.n.toLowerCase(),
        where: `${r.p} ${(r.a ?? []).join(" ")} ${r.i ?? ""}`.toLowerCase(), body: `${r.s} ${r.g ?? ""}`.toLowerCase(),
        url: docs + r.u, weight: member ? 0 : 6 });
    }
    for (const r of data.site) {
      records.push({ kind: "site", data: r, title: r.t, low: r.t.toLowerCase(), where: (r.c ?? "").toLowerCase(),
        body: r.b.toLowerCase(), url: new URL(r.u, siteRoot).href, weight: r.c ? 0 : 2 });
    }
    return records;
  };

  const load = () => {
    loading ??= fetch(new URL("search.json", siteRoot))
      .then((response) => {
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json();
      })
      .then((data) => {
        index = { records: prepare(data), data };
        render();
      })
      .catch(() => {
        loading = null;
        list.replaceChildren();
        status.textContent = "Search could not load its index. Check the connection and open it again.";
      });
    return loading;
  };

  const hitView = (record, terms, id) => {
    const r = record.data;
    const hit = el("a", `search__hit search__hit--${record.kind}`);
    hit.id = id;
    hit.href = record.url;
    hit.tabIndex = -1;
    hit.setAttribute("role", "option");
    hit.setAttribute("aria-selected", "false");
    const head = el("span", "search__head");
    const title = el("span", "search__title");
    const meta = el("span", "search__meta");
    if (record.kind === "api") {
      // nuclei::sync::CancellationToken::cancel shows its owner's path, then its name; a member
      // of one of several impl blocks shows that block's type (Handle<TcpStream>).
      // Every item is in the crate, so its path shows without the leading nuclei::.
      const parent = r.p.slice(0, r.p.length - r.n.length - 2);
      const owner = (r.i ? parent.slice(0, parent.lastIndexOf("::") + 2) + r.i : parent).replace(/^nuclei(::|$)/, "");
      const name = el("span", "search__name");
      name.append(marked(r.n, terms));
      if (owner) title.append(el("span", "search__path", `${owner}::`));
      title.append(name);
      meta.append(el("span", "search__kind", r.k));
      if (r.o) meta.append(el("span", "search__kind search__kind--platform", `${r.o} only`));
    } else {
      title.append(marked(r.t, terms));
      const where = record.kind === "learn"
        ? r.l === 1 ? r.c : [r.n, r.c.replace(/^\d+ /, "")].filter(Boolean).join(" ")
        : r.c ? `Home: ${r.c}` : "Home";
      meta.append(el("span", "search__where", where));
    }
    head.append(title, meta);
    hit.append(head);
    const text = record.kind === "api" ? r.s : r.b && snippet(r.b, terms);
    if (record.kind === "api" && r.g) hit.append(el("span", "search__sig", r.g));
    if (text) {
      const line = el("span", "search__text");
      line.append(marked(text, terms));
      hit.append(line);
    }
    return hit;
  };

  const setActive = (next, scroll = true) => {
    if (options[active]) {
      options[active].classList.remove("is-active");
      options[active].setAttribute("aria-selected", "false");
    }
    active = next;
    const option = options[active];
    if (option) {
      option.classList.add("is-active");
      option.setAttribute("aria-selected", "true");
      input.setAttribute("aria-activedescendant", option.id);
      if (scroll) option.scrollIntoView({ block: "nearest" });
    } else {
      input.removeAttribute("aria-activedescendant");
    }
  };

  const render = () => {
    const query = input.value.trim().toLowerCase().replace(/\s+/g, " ");
    options = [];
    active = -1;
    input.removeAttribute("aria-activedescendant");
    if (!index) {
      list.replaceChildren();
      status.textContent = "Loading the index…";
      return;
    }
    if (!query) {
      const d = index.data;
      const chapters = d.learn.filter((r) => r.l === 1).length;
      const sections = d.learn.length - chapters;
      const items = d.api.filter((r) => !r.u.includes("#") && !memberKinds.has(r.k)).length;
      list.replaceChildren();
      input.setAttribute("aria-expanded", "false");
      status.textContent = `Search ${chapters} Learn pages and their ${sections} sections, ${items} API items and their ${d.api.length - items} members, and the home page.`;
      return;
    }
    const terms = [...new Set(query.split(" "))];
    const found = { learn: [], api: [], site: [] };
    for (const record of index.records) {
      const points = score(record, terms, query);
      if (points) found[record.kind].push([points, record]);
    }
    // The groups keep one order, Learn, then the API, then the home page; hits rank within each.
    const order = Object.keys(found)
      .filter((kind) => found[kind].length)
      .map((kind) => {
        found[kind].sort((a, b) => b[0] - a[0] || a[1].title.length - b[1].title.length || a[1].title.localeCompare(b[1].title));
        return kind;
      });
    const total = order.reduce((n, kind) => n + found[kind].length, 0);
    const views = [];
    for (const kind of order) {
      const all = found[kind];
      const limit = expanded === kind ? all.length : shown[kind];
      const group = el("div", "search__group");
      group.setAttribute("role", "group");
      group.setAttribute("aria-labelledby", `search-group-${kind}`);
      const label = el("p", "search__label");
      label.id = `search-group-${kind}`;
      label.append(el("span", "", groups[kind]), el("span", "search__count", all.length > limit ? `${limit} of ${all.length}` : `${all.length}`));
      group.append(label);
      for (const [, record] of all.slice(0, limit)) {
        const hit = hitView(record, terms, `search-hit-${options.length}`);
        options.push(hit);
        group.append(hit);
      }
      if (all.length > limit) {
        const more = el("a", "search__hit search__more", `Show all ${all.length}`);
        more.id = `search-hit-${options.length}`;
        more.tabIndex = -1;
        more.dataset.more = kind;
        more.setAttribute("role", "option");
        more.setAttribute("aria-selected", "false");
        options.push(more);
        group.append(more);
      }
      views.push(group);
    }
    list.replaceChildren(...views);
    input.setAttribute("aria-expanded", String(total > 0));
    status.textContent = total ? `${total} result${total === 1 ? "" : "s"}` : `No results for “${input.value.trim()}”.`;
    if (options.length) setActive(0, false);
    list.scrollTop = 0;
  };

  const follow = (option, newTab) => {
    if (!option) return;
    if (option.dataset.more) {
      // The row gives way to the rest of its group; the first of them takes its place.
      const at = options.indexOf(option);
      expanded = option.dataset.more;
      render();
      setActive(Math.min(at, options.length - 1));
      return;
    }
    if (newTab) {
      window.open(option.href, "_blank", "noopener");
      return;
    }
    close();
    location.assign(option.href);
  };

  const build = () => {
    dialog = el("dialog", "search");
    dialog.setAttribute("aria-label", "Search Nuclei");
    const bar = el("div", "search__bar");
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    icon.setAttribute("class", "search__icon");
    icon.setAttribute("viewBox", "0 0 16 16");
    icon.setAttribute("aria-hidden", "true");
    icon.innerHTML = '<rect x="2" y="2" width="8.5" height="8.5"/><path d="M10.5 10.5 14 14"/>';
    input = el("input", "search__input");
    Object.assign(input, { type: "text", placeholder: "Search Learn, the API and the home page", autocomplete: "off", spellcheck: false });
    for (const [name, value] of Object.entries({ role: "combobox", "aria-autocomplete": "list", "aria-expanded": "false",
      "aria-controls": "search-results", "aria-label": "Search Learn, the API and the home page", autocapitalize: "off", enterkeyhint: "go" })) {
      input.setAttribute(name, value);
    }
    const shut = el("button", "search__close");
    shut.type = "button";
    shut.setAttribute("aria-label", "Close search");
    shut.append(el("span", "search__close-key", "Esc"), el("span", "search__close-word", "Close"));
    bar.append(icon, input, shut);
    list = el("div", "search__results");
    list.id = "search-results";
    list.setAttribute("role", "listbox");
    list.setAttribute("aria-label", "Results");
    status = el("p", "search__status");
    status.setAttribute("role", "status");
    const help = el("p", "search__help");
    help.setAttribute("aria-hidden", "true");
    help.innerHTML = "<kbd>↑</kbd><kbd>↓</kbd> move <kbd>Enter</kbd> open <kbd>Esc</kbd> close";
    dialog.append(bar, status, list, help);
    document.body.append(dialog);

    input.addEventListener("input", () => {
      expanded = null;
      render();
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        if (options.length) setActive((active + (event.key === "ArrowDown" ? 1 : -1) + options.length) % options.length);
      } else if (event.key === "Enter" && !event.isComposing) {
        event.preventDefault();
        follow(options[active] ?? options[0], event.metaKey || event.ctrlKey);
      }
    });
    // Tab and Shift+Tab go round the dialog's own controls: the input and the close button.
    dialog.addEventListener("keydown", (event) => {
      if (event.key !== "Tab") return;
      event.preventDefault();
      (document.activeElement === input ? shut : input).focus();
    });
    list.addEventListener("click", (event) => {
      const option = event.target.closest("[role=option]");
      if (!option) return;
      if (option.dataset.more || !(event.metaKey || event.ctrlKey || event.shiftKey || event.button === 1)) {
        event.preventDefault();
        follow(option, false);
      }
    });
    list.addEventListener("pointermove", (event) => {
      const option = event.target.closest("[role=option]");
      const at = options.indexOf(option);
      if (at >= 0 && at !== active) setActive(at, false);
    });
    shut.addEventListener("click", close);
    // A click on the backdrop (outside the panel) closes it, as Esc does.
    dialog.addEventListener("click", (event) => {
      if (event.target === dialog) {
        const box = dialog.getBoundingClientRect();
        const inside = event.clientX >= box.left && event.clientX <= box.right && event.clientY >= box.top && event.clientY <= box.bottom;
        if (!inside) close();
      }
    });
    dialog.addEventListener("close", () => {
      if (opener?.isConnected) opener.focus({ preventScroll: true });
      opener = null;
    });
  };

  function open(from) {
    if (!dialog) build();
    if (dialog.open) return;
    opener = from instanceof HTMLElement && from !== document.body ? from : null;
    dialog.showModal();
    input.focus();
    input.select();
    render();
    if (!index) load();
  }

  function close() {
    if (dialog?.open) dialog.close();
  }

  for (const button of document.querySelectorAll("[data-search]")) {
    button.hidden = false;
    button.setAttribute("aria-keyshortcuts", apple ? "Meta+K" : "Control+K");
    const hint = button.querySelector("[data-search-key]");
    if (hint) hint.textContent = apple ? "⌘K" : "Ctrl K";
    button.addEventListener("click", () => open(button));
  }
  addEventListener("keydown", (event) => {
    if (event.key.toLowerCase() !== "k" || event.altKey || event.shiftKey || !(apple ? event.metaKey : event.ctrlKey)) return;
    event.preventDefault();
    if (dialog?.open) close();
    else open(document.activeElement);
  });
}

// A page opened at an anchor lands on it even when fonts arrive during the jump and move
// everything below them: each time a batch of fonts is in (and at the load), the anchor comes
// back into view, until the reader scrolls, types or points.
if (location.hash) {
  const land = () => {
    const target = document.getElementById(decodeURIComponent(location.hash.slice(1)));
    target?.scrollIntoView({ behavior: "instant" });
  };
  const fonts = document.fonts;
  const stop = () => {
    fonts?.removeEventListener("loadingdone", land);
    removeEventListener("load", land);
  };
  for (const type of ["wheel", "touchmove", "keydown", "pointerdown"]) addEventListener(type, stop, { once: true, passive: true });
  fonts?.addEventListener("loadingdone", land);
  addEventListener("load", land, { once: true });
}

// The Learn sidebar on a wide screen. Every chapter is a closed <details> but the current
// page's; one opens at a time (their shared name does that natively). This script animates the
// opening and closing, marks the section being read with an indicator that slides to it and
// stays in view in the sidebar's own scroll, reopens the chapter of the section being read, and
// keeps the address bar on it without new history entries.
const sidebar = document.querySelector(".sidebar");
if (sidebar) {
  const groups = [...sidebar.querySelectorAll("details")];
  const ease = "cubic-bezier(0.16, 1, 0.3, 1)";
  // Where an element sits in the sidebar's scrolled content.
  const offset = (el) => el.getBoundingClientRect().top - sidebar.getBoundingClientRect().top + sidebar.scrollTop;
  const keepInView = (y, height) => {
    const margin = 48;
    if (y < sidebar.scrollTop + margin || y + height > sidebar.scrollTop + sidebar.clientHeight - margin) {
      sidebar.scrollTo({ top: y - sidebar.clientHeight / 3, behavior: still.matches ? "auto" : "smooth" });
    }
  };

  const spy = document.createElement("span");
  spy.className = "sidebar__spy";
  spy.setAttribute("aria-hidden", "true");
  sidebar.append(spy);
  const links = new Map();
  let active = null;

  const placeSpy = () => {
    const link = active && links.get(active.id)?.find((l) => sidebar.contains(l));
    if (!link || link.getClientRects().length === 0) {
      spy.classList.remove("is-on");
      return;
    }
    const y = offset(link);
    const height = link.getBoundingClientRect().height;
    spy.style.transform = `translateY(${y}px) scaleY(${height})`;
    spy.classList.add("is-on");
    keepInView(y, height);
  };

  // Opens one chapter (closing the others) or closes it. The sections that go fade out first;
  // then the change lands at once and every chapter slides from where it was to where it is,
  // while the sections that come fade in. Only transform and opacity move; reduced motion makes
  // it instant.
  let busy = false;
  const setOpen = async (group, open) => {
    if (busy) return;
    busy = true;
    const closing = open ? groups.filter((g) => g !== group && g.open) : [group];
    const motion = !still.matches;
    if (motion) {
      await Promise.all(closing.map((g) => g.querySelector("ol").animate(
        [{ opacity: 1 }, { opacity: 0, transform: "translateY(-4px)" }],
        { duration: 140, easing: "cubic-bezier(0.7, 0, 0.84, 0)" },
      ).finished));
    }
    const before = groups.map((g) => g.getBoundingClientRect().top);
    for (const g of closing) g.open = false;
    if (open) group.open = true;
    placeSpy();
    if (motion) {
      groups.forEach((g, i) => {
        const dy = before[i] - g.getBoundingClientRect().top;
        if (dy) g.animate([{ transform: `translateY(${dy}px)` }, { transform: "none" }], { duration: 300, easing: ease });
      });
      if (open) {
        group.querySelector("ol").animate([{ opacity: 0, transform: "translateY(-6px)" }, { opacity: 1, transform: "none" }], { duration: 300, easing: ease });
      }
    }
    busy = false;
  };

  for (const group of groups) {
    group.querySelector("summary").addEventListener("click", (event) => {
      event.preventDefault();
      setOpen(group, !group.open);
    });
  }

  // The sidebar opens scrolled to the chapter being read.
  const current = sidebar.querySelector("details.is-current");
  if (current) sidebar.scrollTop = offset(current) - sidebar.clientHeight / 4;

  // Scroll spy: the section being read is the last heading above a line at 30% of the window
  // (at the end of the page, any heading on screen). Its links, in the sidebar and in the
  // chapter's own list, get aria-current, and its chapter opens if it was closed. The hash
  // follows once the reader has scrolled, so a page opened at a hash keeps it.
  for (const link of document.querySelectorAll(".sidebar ol a, .toc a")) {
    const url = new URL(link.href);
    if (url.pathname !== location.pathname) continue;
    const id = decodeURIComponent(url.hash.slice(1));
    if (!links.has(id)) links.set(id, []);
    links.get(id).push(link);
  }
  const headings = [...links.keys()].map((id) => document.getElementById(id)).filter(Boolean);
  let atEnd = false;
  let scrolled = false;
  // The reader's own scrolling, not the page's jump to an anchor (a link, a search result),
  // which already names where it goes: until the reader takes over, the address keeps it.
  for (const type of ["wheel", "touchmove", "keydown", "pointerdown"]) {
    addEventListener(type, () => (scrolled = true), { passive: true });
  }
  addEventListener("hashchange", () => (scrolled = false));

  const pick = () => {
    const line = atEnd ? innerHeight : innerHeight * 0.3;
    let next = null;
    for (const heading of headings) if (heading.getBoundingClientRect().top < line) next = heading;
    if (next === active) return;
    for (const link of active ? links.get(active.id) : []) link.removeAttribute("aria-current");
    active = next;
    for (const link of active ? links.get(active.id) : []) link.setAttribute("aria-current", "location");
    const group = active && links.get(active.id).find((l) => sidebar.contains(l))?.closest("details");
    if (group && !group.open) setOpen(group, true);
    else placeSpy();
    if (scrolled && !keeps(active, line)) history.replaceState(history.state, "", active ? `#${active.id}` : location.pathname + location.search);
  };

  // Whether the address names a subsection of the section being read (a heading the sidebar does
  // not list, such as one a search result opened) that the reader is at or past: it stays.
  const keeps = (heading, line) => {
    const target = location.hash && document.getElementById(decodeURIComponent(location.hash.slice(1)));
    if (!heading || !target || target === heading || headings.includes(target)) return target === heading;
    const next = headings[headings.indexOf(heading) + 1];
    const after = (a, b) => Boolean(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
    return after(heading, target) && !(next && after(next, target)) && target.getBoundingClientRect().top < line;
  };

  if (headings.length && "IntersectionObserver" in window) {
    const lines = new IntersectionObserver(pick, { rootMargin: "0px 0px -70% 0px" });
    for (const heading of headings) lines.observe(heading);
    const pager = document.querySelector(".pager");
    if (pager) {
      new IntersectionObserver(([entry]) => {
        atEnd = entry.isIntersecting;
        pick();
      }).observe(pager);
    }
    addEventListener("resize", placeSpy, { passive: true });
  }
}

if (!still.matches && "IntersectionObserver" in window) {
  // Reveals. What is in view at load stays shown and plays only its decorations (reveal-now);
  // what starts below the fold is held back (reveal-wait) and rises in as it arrives. The
  // root reaches far above the viewport, so anything the page jumps past (a link to a
  // section, a restored scroll) counts as arrived and is never left hidden.
  const items = [...document.querySelectorAll("[data-reveal]")];
  const tops = items.map((item) => item.getBoundingClientRect().top);
  const settle = (item, from) => setTimeout(() => item.classList.replace(from, "reveal-done"), 2600);
  const reveal = new IntersectionObserver((entries) => {
    for (const { target, isIntersecting } of entries) {
      if (!isIntersecting) continue;
      reveal.unobserve(target);
      target.classList.replace("reveal-wait", "reveal-in");
      settle(target, "reveal-in");
    }
  }, { rootMargin: "100000px 0px -6% 0px" });
  items.forEach((item, i) => {
    if (tops[i] < innerHeight) {
      item.classList.add("reveal-now");
      settle(item, "reveal-now");
    } else {
      item.classList.add("reveal-wait");
      reveal.observe(item);
    }
  });

  // Loops run only on screen: the hero's footage, dots and tanks (.is-idle when away), the
  // bar's logo, and the proactor diagram, which plays a pass (.is-playing), fades it
  // (.is-clearing) and plays again while it is in view.
  const hero = document.querySelector(".hero");
  const video = hero?.querySelector("video");
  const saveData = navigator.connection?.saveData === true;
  let heroInView = true;
  const playback = () => {
    if (!video) return;
    if (heroInView && !still.matches && !saveData && !document.hidden) {
      // A browser that refuses to start the footage (a power saver, a policy) keeps the
      // poster, which is the designed still frame: nothing to report or retry.
      video.play().catch(() => {});
    } else {
      video.pause();
    }
  };
  const passes = new Map();
  const pass = (flow) => {
    flow.classList.remove("is-clearing");
    flow.classList.add("is-playing");
    passes.set(flow, setTimeout(() => {
      flow.classList.replace("is-playing", "is-clearing");
      passes.set(flow, setTimeout(() => pass(flow), 450));
    }, 8600));
  };
  const rest = (flow) => {
    clearTimeout(passes.get(flow));
    passes.delete(flow);
    flow.classList.remove("is-playing", "is-clearing");
  };
  const sight = new IntersectionObserver((entries) => {
    for (const { target, isIntersecting } of entries) {
      if (target === hero) {
        heroInView = isIntersecting;
        hero.classList.toggle("is-idle", !isIntersecting);
        playback();
      } else if (isIntersecting && !passes.has(target)) {
        pass(target);
      } else if (!isIntersecting) {
        rest(target);
      }
    }
  });
  if (hero) sight.observe(hero);
  for (const flow of document.querySelectorAll(".flow")) sight.observe(flow);
  // The bar's breathing logo waits while the bar is scrolled away.
  const bar = document.querySelector(".nav");
  if (bar) {
    new IntersectionObserver(([entry]) => bar.classList.toggle("is-idle", !entry.isIntersecting)).observe(bar);
  }
  still.addEventListener("change", () => {
    playback();
    if (still.matches) for (const flow of [...passes.keys()]) rest(flow);
  });
  document.addEventListener("visibilitychange", playback);
}
