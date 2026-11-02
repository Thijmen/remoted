// Shown instead of reloading a page whose web process keeps ending (src-tauri/src/webprocess.rs).
// `?url=` is that page: "Try again" opens it, "Open the device list" opens its gateway's start page.
let page = null;
try {
  page = new URL(new URLSearchParams(location.search).get('url') ?? '');
} catch {
  // No page to go back to: only the text is shown.
}

if (page && (page.protocol === 'https:' || page.protocol === 'http:')) {
  const retry = document.getElementById('retry');
  retry.hidden = false;
  retry.addEventListener('click', () => location.assign(page.href));
  if (page.pathname !== '/') {
    const home = document.getElementById('home');
    home.hidden = false;
    home.addEventListener('click', () => location.assign(`${page.origin}/`));
  }
}
