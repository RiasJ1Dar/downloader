// MV3: downloads.onCreated + cancel, далі native host. Не localhost-порт.
const HOST = "com.downloader.host";

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
