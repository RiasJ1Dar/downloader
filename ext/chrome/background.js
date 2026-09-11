// MV3: downloads.onCreated + cancel, далі native host. Не localhost-порт.
const HOST = "com.downloader.host";

function sendToHost(payload) {
  chrome.runtime.sendNativeMessage(HOST, payload, (resp) => {
    if (chrome.runtime.lastError) {
      console.warn("Downloader host:", chrome.runtime.lastError.message);
      return;
    }
    if (resp && resp.ok) {
      console.info("Downloader завдання", resp.id);
    } else {
      console.warn("Downloader:", resp && resp.error);
    }
  });
}

function withCookies(url, referer, extraUrl) {
  const payload = { url: extraUrl || url, referer: referer || url };
  try {
    const u = new URL(payload.url);
    chrome.cookies.getAll({ url: u.origin }, (list) => {
      if (list && list.length) {
        payload.cookies = list.map((c) => c.name + "=" + c.value).join("; ");
      }
      sendToHost(payload);
    });
  } catch (_) {
    sendToHost(payload);
  }
}

const lastManifest = new Map();

chrome.webRequest.onCompleted.addListener(
  (d) => {
    const u = d.url || "";
    if (!/\.m3u8(\?|$)/i.test(u) && !/\.mpd(\?|$)/i.test(u)) return;
    if (d.tabId >= 0) lastManifest.set(d.tabId, u);
  },
  { urls: ["http://*/*", "https://*/*"] }
);

chrome.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || msg.op !== "add") return;
  const tabId = sender.tab && sender.tab.id;
  const manifest = tabId >= 0 ? lastManifest.get(tabId) : undefined;
  const target = manifest || msg.media || msg.page;
  if (!target) return;
  withCookies(msg.page || target, msg.page, target);
});


chrome.downloads.onCreated.addListener((item) => {
  const url = item.url || "";
  if (!url.startsWith("http://") && !url.startsWith("https://")) return;
  if (item.byExtensionId === chrome.runtime.id) return;

  chrome.downloads.cancel(item.id, () => {
    chrome.downloads.erase({ id: item.id });
  });

  const payload = {
    url,
    referer: item.referrer || undefined,
  };

  const send = (cookies) => {
    if (cookies) payload.cookies = cookies;
    chrome.runtime.sendNativeMessage(HOST, payload, (resp) => {
      if (chrome.runtime.lastError) {
        console.warn("Downloader host:", chrome.runtime.lastError.message);
        return;
      }
      if (resp && resp.ok) {
        console.info("Downloader завдання", resp.id);
      } else {
        console.warn("Downloader:", resp && resp.error);
      }
    });
  };

  try {
    const u = new URL(url);
    chrome.cookies.getAll({ url: u.origin }, (list) => {
      if (!list || !list.length) {
        send(undefined);
        return;
      }
      const header = list.map((c) => c.name + "=" + c.value).join("; ");
      send(header);
    });
  } catch (_) {
    send(undefined);
  }
});
