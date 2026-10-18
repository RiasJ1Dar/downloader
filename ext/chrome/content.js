// Кнопка в Shadow DOM: CSS сторінки її не вбиває (урок DLMan/оверлеїв).
(function () {
  const BTN =
    (typeof chrome !== "undefined" &&
      chrome.i18n &&
      chrome.i18n.getMessage("btnDownload")) ||
    "Завантажити";
  const text = (key, fallback) => chrome.i18n.getMessage(key) || fallback;

  // Сніффер потоків через PerformanceObserver (працює для fetch, XHR, MSE)
  try {
    const checkUrl = (url) => {
      if (!url || typeof url !== "string") return;
      if (/\.m3u8(\?|$)/i.test(url) || /\.mpd(\?|$)/i.test(url)) {
        chrome.runtime.sendMessage({ op: "manifest_detected", url: url }, () => {
          const error = chrome.runtime.lastError;
          if (error) console.debug("Downloader:", error.message);
        });
      }
    };

    if (typeof performance !== "undefined" && performance.getEntriesByType) {
      performance.getEntriesByType("resource").forEach((e) => checkUrl(e.name));
    }

    if (typeof PerformanceObserver !== "undefined") {
      const observer = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          checkUrl(entry.name);
        }
      });
      observer.observe({ entryTypes: ["resource"] });
    }
  } catch (error) { console.debug("Downloader: media observer:", error.message); }

  function attach(video) {
    if (video.dataset.dlBtn) return;
    video.dataset.dlBtn = "1";
    const host = document.createElement("div");
    host.style.cssText =
      "position:absolute;z-index:2147483646;pointer-events:none;";
    const shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      :host { color-scheme: light dark; --dl-accent: #9e4417; }
      button, select { font: 13px/1.4 system-ui,sans-serif; min-height: 28px; }
      button { background: var(--dl-accent); color: white; border: 0;
        border-radius: 4px; padding: 6px 10px; cursor: pointer; }
      button:disabled { opacity: .65; cursor: wait; }
      button:focus-visible, select:focus-visible { outline: 3px solid Highlight; outline-offset: 2px; }
      label, [role=status] { display: block; background: Canvas; color: CanvasText;
        font: 13px/1.4 system-ui,sans-serif; padding: 6px; border-radius: 4px;
        box-sizing: border-box; width: 100%;
        max-width: min(320px, calc(100vw - 16px)); overflow-wrap: anywhere; }
      [hidden] { display: none !important; }
      select { display: block; width: 100%; max-width: 100%; margin-top: 4px; }
    `;
    shadow.appendChild(style);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.textContent = BTN;
    btn.style.pointerEvents = "auto";
    const status = document.createElement("div");
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");
    status.hidden = true;
    const picker = document.createElement("label");
    picker.textContent = text("mediaChoice", "Оберіть медіапотік");
    picker.style.pointerEvents = "auto";
    picker.hidden = true;
    const select = document.createElement("select");
    picker.appendChild(select);
    select.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        picker.hidden = true; btn.setAttribute("aria-expanded", "false"); btn.focus();
      }
    });
    const showStatus = (message) => { status.textContent = message; status.hidden = !message; place(); };
    btn.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      if (btn.disabled) return;
      if (!picker.hidden && !select.value) {
        showStatus(text("mediaChoice", "Оберіть медіапотік")); select.focus(); return;
      }
      let media = video.currentSrc || video.src || "";
      if (!media || media.startsWith("blob:")) {
        const source = video.querySelector("source");
        if (source && source.src) media = source.src;
      }
      btn.disabled = true;
      btn.setAttribute("aria-busy", "true");
      btn.textContent = text("btnSending", "Передаю…");
      chrome.runtime.sendMessage({
        op: "add",
        page: location.href,
        media: media.startsWith("http") ? media : "",
        choice: picker.hidden ? undefined : select.value,
      }, (response) => {
        const error = chrome.runtime.lastError;
        btn.disabled = false; btn.removeAttribute("aria-busy"); btn.textContent = BTN;
        if (!error && response?.candidates?.length) {
          select.replaceChildren();
          const placeholder = document.createElement("option");
          placeholder.value = ""; placeholder.textContent = text("mediaChoice", "Оберіть медіапотік");
          select.appendChild(placeholder);
          response.candidates.forEach((candidate, index) => {
            const option = document.createElement("option");
            option.value = candidate.url;
            const url = new URL(candidate.url);
            option.textContent = `${index + 1}. ${candidate.type}: ${url.host}${url.pathname}`;
            select.appendChild(option);
          });
          picker.hidden = false; btn.setAttribute("aria-expanded", "true");
          showStatus(""); select.focus(); place();
        } else if (!error && response?.ok) {
          picker.hidden = true; btn.setAttribute("aria-expanded", "false");
          showStatus(`${text("downloadAdded", "Додано в Downloader")} #${response.id}`);
        } else {
          showStatus(error?.message || response?.error || text("downloadFailed", "Не вдалося передати. Спробуйте ще раз."));
        }
      });
    });
    shadow.appendChild(btn);
    shadow.appendChild(picker);
    shadow.appendChild(status);
    document.documentElement.appendChild(host);

    const place = () => {
      const r = video.getBoundingClientRect();
      if (r.width < 80 || r.height < 40) {
        host.style.display = "none";
        return;
      }
      host.style.display = "block";
      host.style.width = !picker.hidden || !status.hidden ? "min(320px, calc(100vw - 16px))" : "max-content";
      const width = host.getBoundingClientRect().width;
      host.style.left = window.scrollX + Math.max(8, Math.min(r.right - width, window.innerWidth - width - 8)) + "px";
      host.style.top = window.scrollY + Math.max(8, r.top + 8) + "px";
    };
    place();
    video.addEventListener("loadedmetadata", () => {
      picker.hidden = true; btn.setAttribute("aria-expanded", "false"); showStatus(""); place();
    });
    window.addEventListener("scroll", place, { passive: true });
    window.addEventListener("resize", place);
  }

  function scan() {
    document.querySelectorAll("video").forEach(attach);
  }
  scan();
  new MutationObserver(scan).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });
})();
