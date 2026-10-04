"use strict";

const byId = (id) => document.getElementById(id);
const connection = byId("connection");
const allowedConnections = new Set(["openai_api_key", "openai_subscription", "anthropic_api_key"]);
let settings = null;
let busy = false;
let ready = false;

function message(text, isError = false) {
  byId("message").textContent = text;
  byId("message").classList.toggle("error", isError);
}

function rememberConnection() {
  try { localStorage.setItem("tachikoma.connection", connection.value); } catch { /* Storage is optional. */ }
}

async function api(path, body) {
  const options = { cache: "no-store", credentials: "omit" };
  if (body !== undefined) {
    options.method = "POST";
    options.headers = { "Content-Type": "application/json", "X-CSRF-Token": settings.csrf_token };
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  let result;
  try { result = await response.json(); } catch { throw new Error("The local server returned an unexpected response. Reload the page and try again."); }
  if (!response.ok) throw new Error(result.error || "The request could not be completed. Reload the page and try again.");
  return result;
}

function render() {
  ready = false;
  connection.disabled = busy || !settings;
  byId("login").disabled = busy;
  byId("disconnect").disabled = busy;
  byId("subscription-actions").hidden = connection.value !== "openai_subscription";
  if (!settings) { byId("invoke").disabled = true; return; }

  const selected = connection.value;
  let details = "Choose a provider and authentication method to continue.";
  let billing = "Your prompt is sent only when you press Invoke.";
  if (selected === "openai_api_key" || selected === "anthropic_api_key") {
    const openai = selected === "openai_api_key";
    const provider = openai ? settings.openai : settings.anthropic;
    const missing = [];
    if (!provider.api_key_present) missing.push(openai ? "OPENAI_API_KEY" : "ANTHROPIC_API_KEY");
    if (!provider.model) missing.push(openai ? "TACHIKOMA_OPENAI_MODEL" : "TACHIKOMA_ANTHROPIC_MODEL");
    details = missing.length
      ? `Set ${missing.join(" and ")} in the environment, then restart core and reload this page. API keys are entered only in your local environment.`
      : `Ready · ${provider.model}`;
    ready = missing.length === 0;
    billing = `Uses separately billed ${openai ? "OpenAI" : "Anthropic"} API usage. Invoking a prompt may incur charges.`;
  } else if (selected === "openai_subscription") {
    const account = settings.openai.subscription;
    const lines = [];
    billing = "Uses your ChatGPT plan allowance. Manage limits and any additional usage in ChatGPT settings. This connection never switches to API-key billing automatically.";
    byId("disconnect").hidden = !account?.connected;
    byId("login").textContent = account?.connected ? "Review ChatGPT authorization" : "Continue with ChatGPT";
    if (settings.openai.subscription_error) lines.push(settings.openai.subscription_error);
    else if (!account?.connected) lines.push("Continue with ChatGPT to connect your subscription.");
    else if (!account.plan_usage_enabled) lines.push("Connected, but plan usage is not authorized. Review ChatGPT authorization to enable it.");
    else lines.push(`Connected${account.email ? ` as ${account.email}` : ""}.`);
    if (!settings.openai.model) lines.push("Set TACHIKOMA_OPENAI_MODEL to a model available to your account, then restart core and reload this page.");
    else lines.push(`Model: ${settings.openai.model}`);
    details = lines.join(" ");
    ready = Boolean(account?.connected && account.plan_usage_enabled && settings.openai.model);
  }
  byId("billing").textContent = billing;
  byId("configuration").textContent = details;
  byId("invoke").disabled = busy || !ready || !byId("prompt").value.trim();
  byId("invoke").textContent = busy ? "Working…" : "Invoke";
}

connection.addEventListener("change", () => { rememberConnection(); message(""); render(); });
byId("prompt").addEventListener("input", render);

byId("prompt-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (busy || !ready || !byId("prompt").value.trim()) return;
  busy = true;
  rememberConnection();
  render();
  message("Waiting for the selected provider…");
  byId("response").textContent = "";
  byId("response-section").hidden = true;
  try {
    const result = await api("/api/invoke", { connection: connection.value, prompt: byId("prompt").value });
    byId("response").textContent = result.text;
    byId("response-section").hidden = false;
    message("Response received.");
  } catch (error) { message(error.message, true); }
  finally { busy = false; render(); }
});

byId("login").addEventListener("click", async () => {
  if (busy) return;
  busy = true;
  rememberConnection();
  render();
  message("Opening ChatGPT sign-in…");
  try {
    const result = await api("/api/openai/login", {});
    const destination = new URL(result.url);
    if (destination.protocol !== "https:" || destination.hostname !== "auth.openai.com") throw new Error("The sign-in destination could not be verified.");
    window.location.assign(destination.href);
  } catch (error) { message(error.message, true); busy = false; render(); }
});

byId("disconnect").addEventListener("click", async () => {
  if (busy) return;
  busy = true;
  render();
  let notice = "Disconnected from the ChatGPT subscription.";
  let failed = false;
  try { await api("/api/openai/disconnect", {}); }
  catch (error) { notice = error.message; failed = true; }
  try { settings = await api("/api/status"); }
  catch { notice += " Reload this page to check the connection status."; failed = true; }
  busy = false;
  render();
  message(notice, failed);
});

async function initialize() {
  const loginResult = new URLSearchParams(window.location.search).get("login");
  if (loginResult) history.replaceState(null, "", "/");
  try {
    settings = await api("/api/status");
    let saved;
    try { saved = localStorage.getItem("tachikoma.connection"); } catch { /* Storage is optional. */ }
    const preferred = settings.preferred_connection || saved;
    if (allowedConnections.has(preferred)) connection.value = preferred;
    if (loginResult) connection.value = "openai_subscription";
    render();
    if (loginResult === "complete") message("ChatGPT sign-in completed. Check your connection below before invoking.");
    if (loginResult === "failed") message("ChatGPT sign-in was not completed. Try connecting again.", true);
  } catch (error) {
    byId("configuration").textContent = "Connection settings could not be loaded. Check that core is running, then reload this page.";
    message(error.message, true);
  }
}

initialize();
