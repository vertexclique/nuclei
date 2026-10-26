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

// The Learn sidebar is long on a wide screen: it opens scrolled to the chapter being read.
const chapters = document.querySelector(".chapters");
const reading = chapters?.querySelector('[aria-current="page"]');
if (reading && chapters.scrollHeight > chapters.clientHeight) {
  chapters.scrollTop = reading.offsetTop - chapters.clientHeight / 4;
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
