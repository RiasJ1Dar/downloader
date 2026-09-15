// Кнопка в Shadow DOM: CSS сторінки її не вбиває (урок DLMan/оверлеїв).
(function () {
  const BTN =
    (typeof chrome !== "undefined" &&
      chrome.i18n &&
      chrome.i18n.getMessage("btnDownload")) ||
    "Завантажити";

  // Сніффер потоків через PerformanceObserver (працює для fetch, XHR, MSE)
  try {
    const checkUrl = (url) => {
      if (!url || typeof url !== "string") return;
      if (/\.m3u8(\?|$)/i.test(url) || /\.mpd(\?|$)/i.test(url)) {
        chrome.runtime.sendMessage({ op: "manifest_detected", url: url });
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
  } catch (_) {}

  function attach(video) {
    if (video.dataset.dlBtn) return;
    video.dataset.dlBtn = "1";
    const host = document.createElement("div");
    host.style.cssText =
      "position:absolute;z-index:2147483646;pointer-events:none;";
    const shadow = host.attachShadow({ mode: "closed" });
    const btn = document.createElement("button");
    btn.textContent = BTN;
    btn.style.cssText =
      "pointer-events:auto;font:12px/1.2 sans-serif;padding:6px 10px;" +
      "border:0;border-radius:4px;background:#c45c26;color:#fff;cursor:pointer;" +
      "box-shadow:0 1px 4px rgba(0,0,0,.4)";
    btn.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      let media = video.currentSrc || video.src || "";
      if (!media || media.startsWith("blob:")) {
        const source = video.querySelector("source");
        if (source && source.src) media = source.src;
      }
      chrome.runtime.sendMessage({
        op: "add",
        page: location.href,
        media: media.startsWith("http") ? media : "",
      });
    });
    shadow.appendChild(btn);
    document.documentElement.appendChild(host);

    const place = () => {
      const r = video.getBoundingClientRect();
      if (r.width < 80 || r.height < 40) {
        host.style.display = "none";
        return;
      }
      host.style.display = "block";
      host.style.left = window.scrollX + r.right - 118 + "px";
      host.style.top = window.scrollY + r.bottom - 36 + "px";
    };
    place();
    video.addEventListener("loadedmetadata", place);
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
