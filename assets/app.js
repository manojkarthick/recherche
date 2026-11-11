// Keyboard navigation and copy-to-clipboard. Everything else is htmx.
(() => {
  const results = () => [...document.querySelectorAll("#results .result")];
  let cur = -1;

  function select(i) {
    const r = results();
    if (!r.length) return;
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
      const old = btn.textContent;
      btn.textContent = "Copied";
      setTimeout(() => (btn.textContent = old), 1200);
    }
  }

  document.addEventListener("click", (e) => {
    const b = e.target.closest("[data-copy]");
    if (b) {
      e.preventDefault();
      copy(b.dataset.copy, b);
    }
  });

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
      if (resume) copy(resume, (r || document).querySelector(".copy"));
    }
  });
})();
