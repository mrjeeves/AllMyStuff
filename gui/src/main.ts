import { mount, tick } from "svelte";
import App from "./ui/App.svelte";
import "./app.css";

// Surface uncaught errors *in the window itself*. A blank webview — especially a
// secondary popout (the console / room window) where opening devtools is
// awkward — otherwise gives no clue why it failed to paint. This renders the
// error + stack into a fixed overlay so it can be read off the screen (and
// relayed), instead of a silent blank.
function showFatal(label: string, err: unknown): void {
  const detail =
    err instanceof Error ? `${err.name}: ${err.message}\n\n${err.stack ?? ""}` : String(err);
  let el = document.getElementById("fatal-overlay");
  if (!el) {
    el = document.createElement("pre");
    el.id = "fatal-overlay";
    el.style.cssText =
      "position:fixed;inset:0;z-index:2147483647;margin:0;padding:16px;" +
      "background:#1a0000;color:#ff9a9a;font:12px/1.5 ui-monospace,monospace;" +
      "white-space:pre-wrap;overflow:auto;";
    (document.body ?? document.documentElement).appendChild(el);
  }
  el.textContent = `${label}\n\n${detail}`;
}

window.addEventListener("error", (e) => showFatal("Uncaught error", e.error ?? e.message));
window.addEventListener("unhandledrejection", (e) =>
  showFatal("Unhandled promise rejection", e.reason),
);

let app: ReturnType<typeof mount> | undefined;
try {
  app = mount(App, { target: document.getElementById("app")! });
  // The native launcher keeps new windows hidden until their app has mounted.
  // A native HWND alone can survive a failed or terminated webview renderer.
  void tick().then(() => {
    (window as Window & { __ALLMYSTUFF_READY__?: boolean }).__ALLMYSTUFF_READY__ = true;
  });
} catch (e) {
  showFatal("App mount failed", e);
  throw e;
}

export default app;
