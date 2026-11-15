// Keyboard navigation, copy-to-clipboard, votes and the turn index. Everything else is htmx.
(() => {
  const results = () => [...document.querySelectorAll("#results .result")];
  let cur = -1;

  function select(i) {
    const r = results();
    if (!r.length) {
      cur = -1;
      return;
    }
    cur = Math.max(0, Math.min(i, r.length - 1));
    r.forEach((x, j) => x.classList.toggle("sel", j === cur));
    r[cur].scrollIntoView({ block: "nearest" });
  }

  async function copy(text, btn) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Clipboard API needs a secure context; a tailnet http:// URL is not one.
      const t = document.createElement("textarea");
      t.value = text;
      document.body.append(t);
      t.select();
      document.execCommand("copy");
      t.remove();
    }
    if (btn) {
      // Icon-only buttons say "Copied" in their label span; others swap their text.
      const el = btn.querySelector(".lbl") || btn;
      const old = el.textContent;
      el.textContent = "Copied";
      btn.classList.add("done");
      setTimeout(() => {
        el.textContent = old;
        btn.classList.remove("done");
      }, 1200);
    }
  }

  document.addEventListener("click", (e) => {
    const b = e.target.closest("[data-copy]");
    if (b) {
      e.preventDefault();
      copy(b.dataset.copy, b);
    }
  });

  // Show the vote once it's saved. The form stays visible after a vote, not only on hover.
  document.addEventListener("htmx:afterRequest", (e) => {
    const form = e.detail.elt;
    const voted = e.detail.requestConfig?.triggeringEvent?.submitter;
    if (!form.matches?.(".evals") || !e.detail.successful || !voted) return;
    form.querySelectorAll(".btn").forEach((x) => x.setAttribute("aria-pressed", x === voted));
    form.classList.remove("act");
  });

  // Mark the turn being read, in the transcript and in the turn index.
  const turns = [...document.querySelectorAll(".turns > .t")];
  if (turns.length) {
    const links = new Map(
      [...document.querySelectorAll(".toc a")].map((a) => [a.getAttribute("href").slice(1), a.parentElement]),
    );
    let here = null;
    const mark = (t) => {
      if (!t || t === here) return;
      here?.classList.remove("here");
      links.get(here?.id)?.classList.remove("here");
      here = t;
      t.classList.add("here");
      const li = links.get(t.id);
      if (li) {
        li.classList.add("here");
        li.scrollIntoView({ block: "nearest" });
      }
    };
    // A turn jumped to by link or #t… stays marked until the reader scrolls on
    // their own: the last turns can't scroll up to the line below, so the
    // position alone would leave an earlier turn marked.
    let pinned = null;
    const pin = () => {
      const t = location.hash && document.getElementById(location.hash.slice(1));
      if (t && turns.includes(t)) {
        pinned = t;
        mark(t);
      }
    };
    for (const ev of ["wheel", "touchmove", "keydown", "mousedown"]) {
      addEventListener(ev, () => (pinned = null), { passive: true });
    }
    addEventListener("hashchange", pin);
    // Clicking the turn already in the URL fires no hashchange.
    document.querySelector(".toc")?.addEventListener("click", (e) => {
      const t = turns.find((x) => "#" + x.id === e.target.closest("a")?.getAttribute("href"));
      if (t) {
        pinned = t;
        mark(t);
      }
    });
    // The turn being read is the last one whose top has passed the upper third
    // of the window. Turns are in page order, so a binary search finds it.
    const update = () => {
      if (pinned) return mark(pinned);
      // At the bottom nothing more can scroll up, so the last turn in view wins.
      if (innerHeight + scrollY >= document.documentElement.scrollHeight - 2) {
        const last = turns.findLast((t) => t.getBoundingClientRect().top < innerHeight);
        if (last) return mark(last);
      }
      const line = innerHeight / 3;
      let lo = 0;
      let hi = turns.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (turns[mid].getBoundingClientRect().top <= line) lo = mid;
        else hi = mid - 1;
      }
      mark(turns[lo]);
    };
    let queued = false;
    addEventListener("scroll", () => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        update();
      });
    }, { passive: true });
    update();
    pin();
  }

  // Back to top, once the header is out of view.
  const top = document.querySelector(".totop");
  if (top) {
    const head = document.querySelector(".rh");
    const show = () => (top.hidden = head.getBoundingClientRect().bottom > 0);
    addEventListener("scroll", show, { passive: true });
    show();
    top.addEventListener("click", () => {
      scrollTo({ top: 0, behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
      top.blur();
    });
  }

  // A swap replaces the list; keep the selection on the same position.
  document.addEventListener("htmx:afterSwap", () => {
    if (cur >= 0) select(cur);
  });

  document.addEventListener("keydown", (e) => {
    const q = document.getElementById("q");
    const typing = e.target.matches("input, textarea, select");
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "/" && !typing) {
      if (q) {
        e.preventDefault();
        q.focus();
        q.select();
      }
      return;
    }
    if (e.target === q && e.key === "ArrowDown") {
      e.preventDefault();
      q.blur();
      select(0);
      return;
    }
    if (e.key === "Escape") {
      if (typing) {
        e.target.blur();
      } else if (!q) {
        history.length > 1 ? history.back() : (location.href = "/");
      }
      return;
    }
    if (typing) return;
    if (e.key === "j" || e.key === "ArrowDown") {
      e.preventDefault();
      select(cur + 1);
    } else if (e.key === "k" || e.key === "ArrowUp") {
      e.preventDefault();
      select(cur - 1);
    } else if (e.key === "Enter" && cur >= 0) {
      location.href = results()[cur].dataset.href;
    } else if (e.key === "c") {
      const r = cur >= 0 ? results()[cur] : null;
      const resume = r ? r.dataset.resume : document.body.dataset.resume;
      if (resume) copy(resume, (r || document).querySelector(".copy, .cc"));
    }
  });
})();
