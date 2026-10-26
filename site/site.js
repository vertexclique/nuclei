// The site's one script, shared by every page. Nothing on a page depends on it: without it the
// code is plain text to select, every section is shown at rest, and the footage stays on its
// still frame. With it: copy buttons, reveals as sections scroll in, the hero footage, and the
// loops that run only while their section is on screen.

const still = matchMedia("(prefers-reduced-motion: reduce)");

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
  addEventListener("scroll", () => (scrolled = true), { once: true, passive: true });

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
    if (scrolled) history.replaceState(history.state, "", active ? `#${active.id}` : location.pathname + location.search);
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

  // Loops run only on screen: the hero's footage, dots and tanks (.is-idle when away), and
  // the proactor diagram, which plays a pass (.is-playing), fades it (.is-clearing) and
  // plays again while it is in view.
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
  still.addEventListener("change", () => {
    playback();
    if (still.matches) for (const flow of [...passes.keys()]) rest(flow);
  });
  document.addEventListener("visibilitychange", playback);
}
