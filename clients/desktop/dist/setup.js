// The setup page: asks for the gateway, saves it through the app (rd_save_gateway), which then
// opens it. Shown on first start, or with --setup.
const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);

const form = document.getElementById('form');
const url = document.getElementById('url');
const domain = document.getElementById('domain');
const error = document.getElementById('error');
const where = document.getElementById('where');
const button = form.querySelector('button');

invoke('rd_setup_info')
  .then((info) => {
    if (info.gatewayUrl) url.value = info.gatewayUrl;
    if (info.signInDomain) domain.value = info.signInDomain;
    if (info.settingsPath) where.textContent = `Saved in ${info.settingsPath}. Start the app with --setup to change it later.`;
  })
  .catch((e) => console.warn('rd_setup_info', e));

form.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  error.hidden = true;
  button.disabled = true;
  try {
    await invoke('rd_save_gateway', {
      gatewayUrl: url.value,
      signInDomain: domain.value.trim() || null,
    });
    // The app navigates to the gateway.
  } catch (e) {
    error.textContent = String(e);
    error.hidden = false;
    button.disabled = false;
  }
});
