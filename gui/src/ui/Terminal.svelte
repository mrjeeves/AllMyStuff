<script lang="ts">
  // The tabbed terminal for one remote machine — a real shell on the far
  // side (a PTY the host spawns), xterm.js on this side, and the mesh in
  // between. Two skins, one component: the desktop renders it `windowed`
  // (filling a dedicated OS window per machine); the web preview shows the
  // same thing as an in-page popover, where it's honest that live shells
  // need the desktop app.
  //
  // Each tab is its own *session*: its own route (minted by
  // `terminalConnect`, unique per tab) and its own PTY on the far side.
  // A tab's lifecycle is driven by the route's live state from session
  // snapshots — connecting → live (bytes flow) → ended/rejected — plus
  // the host's exit report (`allmystuff://term-exit`) for the code.
  import { onMount } from "svelte";
  import { Terminal as XTerm } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { Unicode11Addon } from "@xterm/addon-unicode11";
  import { WebglAddon } from "@xterm/addon-webgl";
  import { WebLinksAddon } from "@xterm/addon-web-links";
  import "@xterm/xterm/css/xterm.css";
  import { app } from "../store.svelte";
  import {
    clipboardRead,
    clipboardWrite,
    closeThisWindow,
    isMobile,
    onTermExit,
    onTermResize,
    onTerminalSessions,
    onThisWindowClose,
    openExternal,
    termSend,
    watchTerminal,
  } from "../tauri";
  import { displayName, type TerminalSessionInfo } from "../types";
  import TerminalKeys, { type StripKey } from "./TerminalKeys.svelte";

  let {
    host,
    windowed = false,
    initialAttach = null,
  }: { host: string; windowed?: boolean; initialAttach?: string | null } = $props();

  const node = $derived(app.node(host));

  type TabStatus = "connecting" | "live" | "rejected" | "ended" | "offline";
  interface TabMeta {
    id: number;
    routeId: string | null; // null = web mode, nothing can flow
    title: string;
    status: TabStatus;
    /** The line the overlay shows for rejected/ended tabs. */
    note: string;
    /** When this tab was opened by *attaching* to an existing shared shell,
     *  the session id it asked for — passed on the Offer and used to restart
     *  back into the same shared session. null = "new shell". */
    attachSession: string | null;
  }

  /** The non-reactive half of a tab: the emulator and everything that
   *  must be torn down with it. Kept out of `$state` — class instances
   *  aren't proxied, and disposal order matters more than reactivity. */
  interface TabRuntime {
    term: XTerm;
    fit: FitAddon;
    started: boolean; // the route went live and the watcher is wired
    stopWatch: (() => void) | null;
    cleanup: Array<() => void>;
  }

  let tabs = $state<TabMeta[]>([]);
  let activeId = $state(0);
  let bell = $state(false);
  let nextTabId = 1;

  // ---- the mobile key strip (TerminalKeys) -----------------------------
  // On a phone the OS keyboard types into xterm's own textarea; the strip
  // adds what that keyboard can't say (Esc/Tab/Ctrl/arrows) and the tap
  // that summons it. `ctrlArmed` is the one-shot Ctrl: the next character
  // xterm reports is folded into its control byte on the onData path.
  const mobile = isMobile();
  let ctrlArmed = $state(false);
  // How far the OS keyboard lifted the strip — the pane shrinks to match
  // so the shell's bottom rows aren't typing blind behind the keys.
  let keysLift = $state(0);

  /** Fold the one-shot Ctrl into a just-typed character: the classic
   *  terminal mapping (letters and @[\]^_ mask to 0x01–0x1f, space is
   *  NUL). Anything unfoldable passes through — a spent Ctrl never eats
   *  the keystroke. */
  function foldCtrl(s: string): string {
    if (s.length !== 1) return s;
    const c = s === " " ? 0x40 : s.toUpperCase().charCodeAt(0);
    return c >= 0x40 && c <= 0x5f ? String.fromCharCode(c & 0x1f) : s;
  }

  /** A strip key, as the byte sequence the active shell expects — arrows
   *  honour the emulator's application-cursor mode (vim, less, htop), and
   *  an armed Ctrl turns them into the word-jump chords. */
  function stripKey(k: StripKey) {
    const t = tabs.find((x) => x.id === activeId);
    const rt = runtimes.get(activeId);
    if (!t?.routeId || t.status !== "live" || !rt) return;
    const appCursor = !!rt.term.modes.applicationCursorKeysMode;
    const arrow = (letter: string) =>
      ctrlArmed ? `\x1b[1;5${letter}` : appCursor ? `\x1bO${letter}` : `\x1b[${letter}`;
    const seq =
      k === "esc"
        ? "\x1b"
        : k === "tab"
          ? "\t"
          : k === "up"
            ? arrow("A")
            : k === "down"
              ? arrow("B")
              : k === "right"
                ? arrow("C")
                : arrow("D");
    ctrlArmed = false;
    sendBytes(t.routeId, new TextEncoder().encode(seq));
  }

  /** The ⌨ button — a real user gesture, which is what iOS requires to
   *  bring the OS keyboard up for xterm's hidden textarea. */
  function summonKeyboard() {
    runtimes.get(activeId)?.term.focus();
  }
  const runtimes = new Map<number, TabRuntime>();
  // Bumped when an emulator mounts (or disposes). The `runtimes` Map isn't
  // reactive, so the status effect reads this to re-run the instant a
  // tab's emulator is ready — otherwise a route that went live before its
  // pane mounted would wait for the next snapshot poll to get wired.
  let runtimesReady = $state(0);
  let unlistenExit: (() => void) | null = null;
  let unlistenClose: (() => void) | null = null;
  let unlistenSessions: (() => void) | null = null;
  let unlistenResize: (() => void) | null = null;
  let bellTimer: ReturnType<typeof setTimeout> | null = null;

  // ---- "Other Terminals": join an already-open shell -------------------
  // Discover shells already open on this host — started from another window
  // of this machine, or by a fleet member — and join one as a *shared*
  // session (tmux-style: same shell, same scrollback, same keyboard). The
  // local host answers at once; a remote host answers over the mesh (the
  // sessions event). The host's *full* list is the one source of truth; the
  // menu and the per-tab "shared" badge are both derived from it.

  let pickerOpen = $state(false);
  let pickerLoading = $state(false);
  /** Every shell the host currently has open (its answer to our query) —
   *  the single source for the menu and the shared-with badges. */
  let hostSessions = $state<TerminalSessionInfo[]>([]);
  /** The "Other Terminals" button, measured to anchor the (fixed-position)
   *  menu — it must escape the header's `overflow` to be visible at all. */
  let attachBtn = $state<HTMLButtonElement | null>(null);
  /** Viewport coords for the menu's top-right corner, from the button rect. */
  let menuPos = $state({ top: 0, right: 0 });

  /** session id → live attacher count, from the host's list. */
  const attacherCounts = $derived.by(() => {
    const m: Record<string, number> = {};
    for (const s of hostSessions) m[s.session_id] = s.attachers;
    return m;
  });

  /** The session ids this window already shows as live tabs — so the menu
   *  can leave them out and list only the *other* terminals. */
  const mySessionIds = $derived.by(() => {
    const ids = new Set<string>();
    for (const t of tabs) {
      const sid = t.routeId ? app.routeSessions[t.routeId] : undefined;
      if (sid) ids.add(sid);
    }
    return ids;
  });

  /** What the menu lists: the host's open shells minus the ones this very
   *  window already shows — the genuinely *other* terminals to join. */
  const otherSessions = $derived(hostSessions.filter((s) => !mySessionIds.has(s.session_id)));

  /** Pull the host's open shells into {@link hostSessions}. The local host
   *  answers synchronously (and is authoritative — an empty answer means it
   *  truly has none, so we clear); a remote host returns nothing here and
   *  answers asynchronously on the sessions event, so an empty remote answer
   *  leaves the last event-delivered list in place. */
  async function refreshHostSessions() {
    const list = await app.listTerminalSessions(host);
    if (list.length || app.isMe(host)) hostSessions = list;
  }

  /** Toggle the menu; on open, anchor it to the button and (re)query the
   *  host's open shells. */
  async function togglePicker() {
    pickerOpen = !pickerOpen;
    if (!pickerOpen) return;
    if (attachBtn) {
      const r = attachBtn.getBoundingClientRect();
      menuPos = { top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) };
    }
    pickerLoading = true;
    await refreshHostSessions();
    pickerLoading = false;
  }

  /** Join one of the host's other shells in a new tab, then close the menu. */
  function attachTo(session: TerminalSessionInfo) {
    newTab(session.session_id);
    pickerOpen = false;
  }

  /** A human age like "3m" / "2h" for a session created at `unix` seconds. */
  function ageLabel(unix: number): string {
    const secs = Math.max(0, Math.floor(Date.now() / 1000 - unix));
    if (secs < 60) return `${secs}s`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m`;
    if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
    return `${Math.floor(secs / 86400)}d`;
  }

  /** Live attacher count for a tab's shared session, or 0 when solo/unknown. */
  function tabAttachers(t: TabMeta): number {
    if (!t.routeId) return 0;
    const sid = app.routeSessions[t.routeId];
    return sid ? (attacherCounts[sid] ?? 0) : 0;
  }

  // ---- byte plumbing ---------------------------------------------------

  /** Bytes → base64 without blowing the stack on a huge paste. */
  function b64encode(bytes: Uint8Array): string {
    let bin = "";
    const STEP = 0x8000;
    for (let i = 0; i < bytes.length; i += STEP) {
      bin += String.fromCharCode(...bytes.subarray(i, i + STEP));
    }
    return btoa(bin);
  }

  function sendBytes(routeId: string, bytes: Uint8Array) {
    void termSend(routeId, { kind: "data", bytes: b64encode(bytes) }).catch(() => {
      // A send into a just-torn route; the status overlay tells the story.
    });
  }

  // ---- tab lifecycle ---------------------------------------------------

  /** Open a new tab. With `session` set it *attaches* to that already-running
   *  shared shell on the host (tmux-style — same scrollback, same keyboard);
   *  without it, a fresh shell is minted. */
  function newTab(session?: string) {
    const id = nextTabId++;
    const routeId = app.terminalConnect(host, session);
    console.debug(
      `[terminal] tab ${id} opened — route ${routeId ?? "(none: web mode)"}` +
        (session ? ` attaching to ${session}` : ""),
    );
    tabs.push({
      id,
      routeId,
      title: session ? `↳ ${session}` : `Shell ${id}`,
      status: routeId ? "connecting" : "offline",
      note: routeId ? "" : "Live terminals need the desktop app.",
      attachSession: session ?? null,
    });
    activeId = id;
  }

  /** Svelte action: a tab's pane is in the DOM — build its emulator. A
   *  failure here must *show itself* (the tab would otherwise sit on
   *  "connecting" forever with a perfectly live route behind it). */
  function mountTerm(el: HTMLElement, tabId: number) {
    const meta = tabs.find((t) => t.id === tabId);
    if (!meta) return;
    try {
      return mountTermInner(el, tabId, meta);
    } catch (e) {
      console.error("[terminal] emulator failed to start:", e);
      meta.status = "rejected";
      meta.note = `the terminal emulator failed to start here: ${e}`;
      return;
    }
  }

  function mountTermInner(el: HTMLElement, tabId: number, meta: TabMeta) {
    const term = new XTerm({
      allowProposedApi: true,
      cursorBlink: true,
      scrollback: 10_000,
      fontSize: 13,
      fontFamily:
        "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace",
      macOptionIsMeta: true,
      theme: {
        background: "#14121f",
        foreground: "#d7d2ec",
        // The accent magenta — oklch(0.64 0.255 350), same as the rest of
        // the app (xterm wants plain hex).
        cursor: "#f11ea1",
        selectionBackground: "#3d3760",
      },
      // The remote's ConPTY reflows on resize; telling xterm gets the
      // heuristics (cursor handling, wrapped-line tracking) right.
      windowsPty:
        node?.summary?.os === "windows" ? { backend: "conpty" } : undefined,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = "11";
    term.loadAddon(
      new WebLinksAddon((e, uri) => {
        e.preventDefault();
        void openExternal(uri);
      }),
    );

    const rt: TabRuntime = { term, fit, started: false, stopWatch: null, cleanup: [] };
    runtimes.set(tabId, rt);
    runtimesReady++; // nudge the status effect — this tab can wire now

    term.open(el);
    // GPU rendering where the webview offers WebGL2; the DOM renderer is
    // the silent fallback (WebKitGTK without GL, lost contexts, …). The
    // addon needs an opened terminal — load it strictly after `open`.
    try {
      const gl = new WebglAddon();
      gl.onContextLoss(() => gl.dispose());
      term.loadAddon(gl);
    } catch (e) {
      console.info("[terminal] WebGL renderer unavailable, using DOM:", e);
    }
    fit.fit();
    term.focus();

    // Keystrokes → the far PTY. `onData` is everything typed (incl. escape
    // sequences); `onBinary` is the rare raw-byte path (some mouse modes).
    rt.cleanup.push(
      term.onData((s) => {
        const t = tabs.find((x) => x.id === tabId);
        if (!t?.routeId || t.status !== "live") return;
        let data = s;
        // The strip's one-shot Ctrl discharges on the next thing typed,
        // foldable or not — sticky keys never stay stuck.
        if (ctrlArmed) {
          data = foldCtrl(s);
          ctrlArmed = false;
        }
        sendBytes(t.routeId, new TextEncoder().encode(data));
      }).dispose,
    );
    rt.cleanup.push(
      term.onBinary((s) => {
        const t = tabs.find((x) => x.id === tabId);
        if (!t?.routeId || t.status !== "live") return;
        const bytes = new Uint8Array(s.length);
        for (let i = 0; i < s.length; i++) bytes[i] = s.charCodeAt(i) & 0xff;
        sendBytes(t.routeId, bytes);
      }).dispose,
    );

    // Report this window's *capacity* (the cols/rows it could show) to the
    // host, debounced so a drag doesn't machine-gun the channel. The host
    // reconciles the shared PTY to the smallest attached window and sends back
    // the authoritative size this emulator actually renders at (handled in
    // onMount). Capacity (reported) and render size (received) stay decoupled,
    // so a bigger window letterboxes to the shared size instead of wrapping
    // wrong — and the smallest window still drives the shell's size.
    let resizeTimer: ReturnType<typeof setTimeout> | null = null;
    const reportCapacitySoon = () => {
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        const t = tabs.find((x) => x.id === tabId);
        const d = fit.proposeDimensions();
        if (t?.routeId && t.status === "live" && d && d.cols > 0 && d.rows > 0)
          void termSend(t.routeId, { kind: "resize", cols: d.cols, rows: d.rows }).catch(() => {});
      }, 80);
    };
    rt.cleanup.push(() => {
      if (resizeTimer) clearTimeout(resizeTimer);
    });

    // The shell names the tab (OSC 0/2 — what most prompts set).
    rt.cleanup.push(
      term.onTitleChange((title) => {
        const t = tabs.find((x) => x.id === tabId);
        if (t && title.trim()) t.title = title.trim();
      }).dispose,
    );

    rt.cleanup.push(
      term.onBell(() => {
        bell = true;
        if (bellTimer) clearTimeout(bellTimer);
        bellTimer = setTimeout(() => (bell = false), 150);
      }).dispose,
    );

    // Terminal-convention clipboard chords (Ctrl+C must stay SIGINT):
    // Ctrl+Shift+C copies the selection, Ctrl+Shift+V pastes, and
    // Ctrl+Shift+T opens a sibling shell. macOS Cmd+C/V already work —
    // xterm answers the webview's native copy/paste events.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown" || !e.ctrlKey || !e.shiftKey) return true;
      const k = e.key.toLowerCase();
      if (k === "c" && term.hasSelection()) {
        void clipboardWrite(term.getSelection());
        term.clearSelection();
        return false;
      }
      if (k === "v") {
        void clipboardRead().then((text) => text && term.paste(text));
        return false;
      }
      if (k === "t") {
        newTab();
        return false;
      }
      return true;
    });

    // Window grew/shrank → re-report capacity (not a local refit: the render
    // size is the host's authoritative shared size, applied via term-resize).
    const ro = new ResizeObserver(() => {
      if (el.clientWidth > 0 && el.clientHeight > 0) reportCapacitySoon();
    });
    ro.observe(el);
    rt.cleanup.push(() => ro.disconnect());

    if (meta.status === "offline") {
      term.write("\x1b[2m[demo mode — live terminals need the desktop app]\x1b[0m\r\n");
    }

    return {
      destroy() {
        disposeRuntime(tabId);
      },
    };
  }

  function disposeRuntime(tabId: number) {
    const rt = runtimes.get(tabId);
    if (!rt) return;
    runtimes.delete(tabId);
    runtimesReady++;
    rt.stopWatch?.();
    for (const fn of rt.cleanup) fn();
    rt.term.dispose();
  }

  /** A tab's route went live: wire the byte stream and tell the host this
   *  window's capacity (the host reconciles and sends back the size to render
   *  at). Buffered output (the prompt that raced the window boot) arrives on
   *  the first poll. */
  function startSession(meta: TabMeta, rt: TabRuntime) {
    if (rt.started || !meta.routeId) return;
    rt.started = true;
    const routeId = meta.routeId;
    console.debug(`[terminal] wiring output for ${routeId}`);
    void watchTerminal(routeId, (bytes) => rt.term.write(bytes)).then((stop) => {
      // The tab may have died while the watch was being wired.
      if (runtimes.get(meta.id) !== rt) {
        stop();
        return;
      }
      rt.stopWatch = stop;
    });
    const d = rt.fit.proposeDimensions();
    if (d && d.cols > 0 && d.rows > 0)
      void termSend(routeId, { kind: "resize", cols: d.cols, rows: d.rows }).catch(() => {});
    rt.term.focus();
  }

  // Drive each tab's status off its route's live state from the session
  // snapshots — the same source of truth the rest of the app reads. The
  // status flips on the route alone; wiring the byte stream additionally
  // needs the emulator runtime, and is retried each pass so a slow mount
  // can never strand a live route on "connecting".
  //
  // Both reactive sources are read *unconditionally up front*: an effect's
  // dependencies are whatever it read on its last run, so reading them
  // only inside the `tabs` loop meant an empty first run (before onMount's
  // first tab) subscribed to neither — and then only a `tabs` change, like
  // opening a *second* tab, would ever re-run it. That's why the first tab
  // sat on "connecting" until a sibling was added.
  $effect(() => {
    const states = app.routeStates;
    void runtimesReady;
    for (const t of tabs) {
      if (!t.routeId) continue;
      const st = states[t.routeId];
      const rt = runtimes.get(t.id);
      if (st?.state === "active") {
        if (t.status === "connecting") {
          console.debug(`[terminal] route live: ${t.routeId}`);
          t.status = "live";
        }
        if (t.status === "live" && rt && !rt.started) startSession(t, rt);
      } else if (st?.state === "rejected") {
        if (t.status !== "rejected") {
          console.warn(`[terminal] route rejected: ${t.routeId} (${st.reason ?? "no reason"})`);
          t.status = "rejected";
          t.note = st.reason || "the far side refused the session";
          rt?.term.write(`\r\n\x1b[31m[offer rejected: ${t.note}]\x1b[0m\r\n`);
        }
      } else if (st?.state === "torn_down" && (t.status === "live" || t.status === "connecting")) {
        // The route closed — under a live shell (host app quit, far side
        // ended it) or before it ever delivered one (the host's shell
        // failed to spawn). An exit report usually lands first and is the
        // better story; this is the fallback either way.
        console.debug(`[terminal] route torn down: ${t.routeId} (was ${t.status})`);
        const early = t.status === "connecting";
        t.status = "ended";
        if (!t.note)
          t.note = early
            ? "the far side closed the session before it started"
            : "session ended by the far side";
      }
    }
  });

  /** Give a finished tab a fresh shell (new route, same scrollback). */
  function restartTab(tabId: number) {
    const t = tabs.find((x) => x.id === tabId);
    const rt = runtimes.get(tabId);
    if (!t || !rt) return;
    rt.stopWatch?.();
    rt.stopWatch = null;
    rt.started = false;
    // Restart back into the same shared session when this tab was attached;
    // otherwise mint a fresh shell.
    t.routeId = app.terminalConnect(host, t.attachSession ?? undefined);
    t.status = t.routeId ? "connecting" : "offline";
    t.note = t.routeId ? "" : "Live terminals need the desktop app.";
    rt.term.write("\r\n\x1b[2m── new session ──\x1b[0m\r\n");
    rt.term.focus();
  }

  async function closeTab(tabId: number) {
    const t = tabs.find((x) => x.id === tabId);
    if (!t) return;
    const teardown = t.routeId ? app.terminalDisconnect(t.routeId) : Promise.resolve();
    tabs = tabs.filter((x) => x.id !== tabId); // unmounts the pane → disposeRuntime
    if (activeId === tabId && tabs.length) activeId = tabs[tabs.length - 1].id;
    if (tabs.length === 0) {
      await endAll(teardown);
      return;
    }
    void teardown;
    focusActiveSoon();
  }

  function selectTab(tabId: number) {
    activeId = tabId;
    focusActiveSoon();
  }

  /** Pop this tab out into its own terminal window — the terminal twin of a
   *  video feed's ⧉ pop-out. The shell is shared (tmux-style), so the new
   *  window re-attaches to the same session while we detach this one by closing
   *  the tab; the shell and its scrollback carry straight on. Only meaningful
   *  once the host has resolved the session id (so the new window can name it). */
  function popOutTab(tabId: number) {
    const t = tabs.find((x) => x.id === tabId);
    if (!t || !t.routeId) return;
    const session = app.routeSessions[t.routeId];
    if (!session) {
      app.toast("warn", "This shell isn't ready to pop out yet — give it a moment.");
      return;
    }
    app.popOutTerminal(host, session);
    void closeTab(tabId);
  }

  /** After a tab becomes visible its pane has real dimensions again — re-report
   *  capacity (the host may grow the shared size back) and focus, next frame.
   *  The render size stays the host's authoritative one, never a local refit. */
  function focusActiveSoon() {
    requestAnimationFrame(() => {
      const rt = runtimes.get(activeId);
      const t = tabs.find((x) => x.id === activeId);
      if (rt) {
        const d = rt.fit.proposeDimensions();
        if (t?.routeId && t.status === "live" && d && d.cols > 0 && d.rows > 0)
          void termSend(t.routeId, { kind: "resize", cols: d.cols, rows: d.rows }).catch(() => {});
        rt.term.focus();
      }
    });
  }

  let closing = false;
  /** Tear every session down and close the surface. Bounded — a wedged
   *  backend must never hold a closing window hostage. */
  async function endAll(extra?: Promise<unknown>) {
    if (closing) return;
    closing = true;
    const teardowns = tabs
      .filter((t) => t.routeId)
      .map((t) => app.terminalDisconnect(t.routeId!));
    if (extra) teardowns.push(extra as Promise<never>);
    tabs = [];
    if (windowed) {
      await Promise.race([Promise.allSettled(teardowns), new Promise((r) => setTimeout(r, 600))]);
      void closeThisWindow();
    } else {
      app.closeTerminal();
    }
    closing = false;
  }

  function onPaneContextMenu(e: MouseEvent) {
    // Terminal convention: right-click copies the selection if there is
    // one, otherwise pastes — never the browser menu over a shell.
    e.preventDefault();
    const rt = runtimes.get(activeId);
    if (!rt) return;
    if (rt.term.hasSelection()) {
      void clipboardWrite(rt.term.getSelection());
      rt.term.clearSelection();
    } else {
      void clipboardRead().then((text) => text && rt.term.paste(text));
    }
  }

  onMount(() => {
    // A popped-out window joins the shared shell it was opened for; an ordinary
    // window mints a fresh one.
    newTab(initialAttach ?? undefined);
    // Tab statuses hang off route negotiation states from session
    // snapshots. Snapshot *events* are the latency win but can be lost
    // (this codebase's video plane moved to pull for exactly that
    // reason) — so pull the snapshot on a short interval as the truth.
    const sessionPoll = setInterval(() => void app.refreshSession(), 1000);
    void onTermExit((ev) => {
      const t = tabs.find((x) => x.routeId === ev.route);
      if (!t) return;
      const rt = runtimes.get(t.id);
      const what = ev.code === null ? "ended" : `exited with code ${ev.code}`;
      t.status = "ended";
      t.note = `process ${what}`;
      rt?.term.write(`\r\n\x1b[2m[${t.note}]\x1b[0m\r\n`);
    }).then((u) => (unlistenExit = u));
    if (windowed) {
      // The OS chrome's ✕ must tear the sessions down too — the close is
      // held until the teardowns are on the wire (see onThisWindowClose).
      void onThisWindowClose(() => void endAll()).then((u) => (unlistenClose = u));
    }
    // A remote host's answer to a sessions query (and its own pushes after a
    // tab attaches/detaches) — feeds the one source of truth, which keeps the
    // open menu and the "shared · N" badges live.
    void onTerminalSessions((ev) => {
      if (app.isSameMachine(ev.from, host)) hostSessions = ev.sessions;
    }).then((u) => (unlistenSessions = u));
    // The host's authoritative shared-PTY size for a route — render this tab's
    // emulator at it (letterboxing a bigger window) so a shared shell wraps
    // identically for everyone. The window keeps reporting its own capacity
    // (above); the smallest attached window is what the host settles on.
    void onTermResize((ev) => {
      const t = tabs.find((x) => x.routeId === ev.route);
      if (!t) return;
      const rt = runtimes.get(t.id);
      if (rt && ev.cols > 0 && ev.rows > 0 && (rt.term.cols !== ev.cols || rt.term.rows !== ev.rows))
        rt.term.resize(ev.cols, ev.rows);
    }).then((u) => (unlistenResize = u));
    // Keep the list current on a slow cadence — while the menu is open (so a
    // shell another window/peer just opened shows up) or whenever a tab is on
    // a known session (so its shared badge stays live). The count only moves
    // when someone attaches/detaches, so this is cheap and off any hot path.
    const countPoll = setInterval(() => {
      if (pickerOpen || tabs.some((t) => t.routeId && app.routeSessions[t.routeId])) {
        void refreshHostSessions();
      }
    }, 4000);
    return () => {
      clearInterval(sessionPoll);
      clearInterval(countPoll);
      unlistenExit?.();
      unlistenClose?.();
      unlistenSessions?.();
      unlistenResize?.();
      if (bellTimer) clearTimeout(bellTimer);
      // The native window-close path normally clears `tabs` through endAll.
      // A parent remount/navigation is different: Svelte destroys us directly.
      // Retire those routes too so a UI lifecycle blip cannot leave thousands
      // of offered/active terminal sessions in the backend.
      for (const t of tabs) {
        if (t.routeId) void app.terminalDisconnect(t.routeId);
      }
    };
  });
</script>

{#if node}
  <div class="scrim" class:windowed>
    {#if !windowed}
      <button class="backdrop" aria-label="Close terminal" onclick={() => void endAll()}></button>
    {/if}
    <div
      class="terminal"
      class:bell
      role="dialog"
      aria-modal={!windowed}
      aria-label="Terminal on {displayName(node)}"
    >
      <header class="head">
        <div class="who">
          <span class="ico">📟</span>
          <div class="meta">
            <div class="name">{displayName(node)}</div>
            <div class="sub">
              <span class="dot" class:on={node.online}></span>
              {node.online ? "online" : "offline"} · terminal
            </div>
          </div>
        </div>
        <div class="tabs" role="tablist" aria-label="Shells">
          {#each tabs as t (t.id)}
            {@const shared = tabAttachers(t)}
            <div class="tab" class:active={t.id === activeId}>
              <button
                class="tab-pick"
                role="tab"
                aria-selected={t.id === activeId}
                title={shared > 1 ? `${t.title} · shared with ${shared - 1} other` : t.title}
                onclick={() => selectTab(t.id)}
              >
                <span class="tab-state {t.status}"></span>
                <span class="tab-label">{t.title}</span>
                {#if shared > 1}
                  <span class="tab-shared" title="Shared session — {shared} viewers attached"
                    >👥{shared}</span
                  >
                {/if}
              </button>
              {#if windowed && t.routeId && app.routeSessions[t.routeId]}
                <button
                  class="tab-pop"
                  title="Pop this shell out into its own window"
                  onclick={() => popOutTab(t.id)}
                >
                  ⧉
                </button>
              {/if}
              <button class="tab-x" title="Close this shell" onclick={() => void closeTab(t.id)}>
                ✕
              </button>
            </div>
          {/each}
          <button class="tab-new" title="New shell (Ctrl+Shift+T)" onclick={() => newTab()}>＋</button>
          <div class="picker">
            <button
              bind:this={attachBtn}
              class="tab-attach"
              class:open={pickerOpen}
              title="Join a shell already open on {displayName(node)} (shared, tmux-style)"
              aria-expanded={pickerOpen}
              onclick={() => void togglePicker()}
            >
              👥 Other Terminals{#if otherSessions.length > 0}<span class="attach-count"
                  >{otherSessions.length}</span
                >{/if}
            </button>
            {#if pickerOpen}
              <!-- Click-away backdrop, then the menu above it. -->
              <button
                class="picker-scrim"
                aria-label="Close the other-terminals menu"
                onclick={() => (pickerOpen = false)}
              ></button>
              <div
                class="picker-menu"
                role="menu"
                aria-label="Other open terminals"
                style="top: {menuPos.top}px; right: {menuPos.right}px;"
              >
                <div class="picker-head">Other shells open on {displayName(node)}</div>
                {#if pickerLoading}
                  <div class="picker-empty">Looking…</div>
                {:else if otherSessions.length === 0}
                  <div class="picker-empty">
                    {#if hostSessions.length > 0}
                      Every shell open here is already a tab in this window. Open another window — or
                      have a fleet member open one — and it'll show up here to join.
                    {:else if app.isMe(host)}
                      No other shells open on this machine yet. Open one from another terminal window
                      (here or from a fleet member) and it'll appear here — joining shares the shell,
                      its scrollback and its keyboard.
                    {:else}
                      No other shells open on {displayName(node)} yet — or it's running an older
                      AllMyStuff that can't share them. Shells started there (by its owner or a fleet
                      member) show up here to join.
                    {/if}
                  </div>
                {:else}
                  {#each otherSessions as s (s.session_id)}
                    <button class="picker-row" role="menuitem" onclick={() => attachTo(s)}>
                      <span class="picker-title">{s.title}</span>
                      <span class="picker-meta">
                        {#if s.attachers > 0}<span class="picker-att">👥 {s.attachers}</span>{/if}
                        <span class="picker-age">{ageLabel(s.created_unix)}</span>
                      </span>
                    </button>
                  {/each}
                {/if}
              </div>
            {/if}
          </div>
        </div>
        <button class="x" onclick={() => void endAll()} aria-label="Close">✕</button>
      </header>

      <!-- role=application: a shell surface — keys belong to the far
           machine; right-click is copy/paste, never the browser menu.
           On mobile the stage keeps clear of the fixed key strip (and of
           the OS keyboard the strip is riding) — the ResizeObserver refits
           the emulator as the padding moves. -->
      <div
        class="stage"
        role="application"
        aria-label="Shell on {displayName(node)}"
        style:padding-bottom={mobile ? `calc(2.9rem + ${keysLift}px)` : null}
        oncontextmenu={onPaneContextMenu}
      >
        {#each tabs as t (t.id)}
          <div class="pane" class:hidden={t.id !== activeId}>
            <div class="xterm-host" use:mountTerm={t.id}></div>
            {#if t.status === "connecting"}
              <div class="veil">
                <p>Connecting to <b>{displayName(node)}</b>…</p>
                <!-- The raw negotiation state, so a stall names its stage:
                     "offered" = the far side never answered; "not
                     negotiated yet" = this route is missing from the
                     snapshot. Global route history is unrelated noise here. -->
                <p class="diag">
                  route {app.routeStates[t.routeId ?? ""]?.state ?? "not negotiated yet"}
                </p>
              </div>
            {:else if t.status === "offline"}
              <div class="veil">
                <p>{t.note}</p>
              </div>
            {:else if t.status === "rejected" || t.status === "ended"}
              <div class="veil ended">
                <p>{t.status === "rejected" ? "Refused: " : ""}{t.note}</p>
                <div class="veil-actions">
                  {#if t.status === "ended"}
                    <button class="btn-restart" onclick={() => restartTab(t.id)}>↻ New session</button>
                  {/if}
                  <button class="btn-close" onclick={() => void closeTab(t.id)}>Close tab</button>
                </div>
              </div>
            {/if}
          </div>
        {/each}
      </div>

      {#if mobile}
        <TerminalKeys
          ctrl={ctrlArmed}
          ontogglectrl={() => (ctrlArmed = !ctrlArmed)}
          onkey={stripKey}
          onkeyboard={summonKeyboard}
          onlift={(px) => (keysLift = px)}
        />
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    /* Same layer as the console + files popovers: with the z-indexes
       tied, DOM order in App.svelte (console → terminal → files) decides
       the stack, so a terminal opened from the console's footer lands
       above the console instead of invisibly behind it. */
    z-index: 60;
    display: grid;
    place-items: center;
  }
  .scrim.windowed {
    position: absolute;
    background: #14121f;
  }
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: rgba(20, 18, 31, 0.55);
    cursor: default;
  }
  .terminal {
    position: relative;
    display: flex;
    flex-direction: column;
    width: min(960px, 94vw);
    height: min(620px, 88vh);
    background: #14121f;
    border-radius: 12px;
    box-shadow: 0 18px 60px rgba(10, 8, 20, 0.55);
    overflow: hidden;
    border: 1px solid #2c2745;
    transition: border-color 0.12s ease;
  }
  .terminal.bell {
    border-color: var(--accent);
  }
  .windowed .terminal {
    width: 100%;
    height: 100%;
    border-radius: 0;
    border: none;
    box-shadow: none;
  }
  /* Phone-width: the in-page terminal takes the whole screen (the scrim is
     already above the header + portrait pill dock at z 60). Safe-area
     padding keeps the tab bar's ✕ out from under the status bar. Zero
     effect on desktop: env() is 0 there, and a narrow *windowed* terminal
     is already 100%×100%. */
  .head {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.45rem 0.6rem;
    background: #1c1930;
    border-bottom: 1px solid #2c2745;
    flex-shrink: 0;
    min-width: 0;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    min-width: 0;
  }
  .ico {
    font-size: 1.25rem;
  }
  .meta .name {
    color: #e8e4f8;
    font-size: 0.86rem;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 13rem;
  }
  .meta .sub {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    color: #8f88ad;
    font-size: 0.68rem;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #5c5680;
  }
  .dot.on {
    background: #41c98d;
  }
  .tabs {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    flex: 1;
    min-width: 0;
    overflow-x: auto;
    scrollbar-width: thin;
  }
  .tab {
    display: flex;
    align-items: center;
    background: #242040;
    border: 1px solid transparent;
    border-radius: 7px;
    flex-shrink: 0;
  }
  .tab.active {
    background: #322c55;
    border-color: #494173;
  }
  .tab-pick {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    border: none;
    background: none;
    color: #b9b2d6;
    font-size: 0.74rem;
    padding: 0.28rem 0.15rem 0.28rem 0.55rem;
    cursor: pointer;
    max-width: 11rem;
  }
  .tab.active .tab-pick {
    color: #efecfb;
  }
  .tab-label {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tab-state {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex-shrink: 0;
    background: #5c5680;
  }
  .tab-state.live {
    background: #41c98d;
  }
  .tab-state.connecting {
    background: #d8a544;
  }
  .tab-state.rejected,
  .tab-state.ended {
    background: #d4587a;
  }
  .tab-x {
    border: none;
    background: none;
    color: #7d76a0;
    font-size: 0.62rem;
    width: 1.3rem;
    height: 1.3rem;
    border-radius: 50%;
    cursor: pointer;
    margin-right: 0.15rem;
  }
  .tab-x:hover {
    background: #463e6e;
    color: #f1eefb;
  }
  .tab-pop {
    border: none;
    background: none;
    color: #7d76a0;
    font-size: 0.72rem;
    width: 1.3rem;
    height: 1.3rem;
    border-radius: 50%;
    cursor: pointer;
  }
  .tab-pop:hover {
    background: #463e6e;
    color: #f1eefb;
  }
  .tab-new {
    border: 1px dashed #494173;
    background: none;
    color: #9a93b8;
    width: 1.65rem;
    height: 1.65rem;
    border-radius: 7px;
    font-size: 0.8rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .tab-new:hover {
    color: #efecfb;
    border-color: #6f64ab;
  }
  .tab-shared {
    font-size: 0.62rem;
    color: #9fd3ff;
    background: #21344a;
    border-radius: 5px;
    padding: 0 0.22rem;
    flex-shrink: 0;
  }
  .picker {
    position: relative;
    flex-shrink: 0;
  }
  .tab-attach {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    border: 1px solid #494173;
    background: none;
    color: #b9b2d6;
    height: 1.65rem;
    border-radius: 7px;
    font-size: 0.72rem;
    padding: 0 0.5rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .tab-attach:hover,
  .tab-attach.open {
    color: #efecfb;
    border-color: #6f64ab;
    background: #2a2548;
  }
  .attach-count {
    font-size: 0.62rem;
    color: #9fd3ff;
    background: #21344a;
    border-radius: 5px;
    padding: 0 0.26rem;
    line-height: 1.4;
  }
  .picker-scrim {
    position: fixed;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
    z-index: 70;
  }
  .picker-menu {
    /* Fixed, not absolute: the header (`.terminal` overflow:hidden, `.tabs`
       overflow:auto) would otherwise clip a dropdown that escapes the tab
       strip — it rendered but was invisible. Anchored to the button's
       measured rect (see `menuPos`). */
    position: fixed;
    z-index: 71;
    width: 19rem;
    max-height: 60vh;
    overflow-y: auto;
    background: #1c1930;
    border: 1px solid #3a3460;
    border-radius: 9px;
    box-shadow: 0 14px 40px rgba(10, 8, 20, 0.6);
    padding: 0.3rem;
  }
  .picker-head {
    color: #9a93b8;
    font-size: 0.68rem;
    padding: 0.3rem 0.4rem 0.4rem;
    border-bottom: 1px solid #2c2745;
    margin-bottom: 0.25rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .picker-empty {
    color: #8f88ad;
    font-size: 0.74rem;
    line-height: 1.45;
    padding: 0.5rem 0.45rem;
  }
  .picker-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    width: 100%;
    border: none;
    background: none;
    color: #d7d2ec;
    border-radius: 6px;
    padding: 0.4rem 0.45rem;
    cursor: pointer;
    text-align: left;
  }
  .picker-row:hover {
    background: #2c2750;
  }
  .picker-title {
    font-size: 0.78rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .picker-meta {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    flex-shrink: 0;
    font-size: 0.68rem;
    color: #8f88ad;
  }
  .picker-att {
    color: #9fd3ff;
  }
  .x {
    border: none;
    background: #242040;
    color: #b9b2d6;
    width: 1.8rem;
    height: 1.8rem;
    border-radius: 50%;
    font-size: 0.75rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .x:hover {
    background: #463e6e;
    color: #fff;
  }
  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .pane {
    position: absolute;
    inset: 0;
    display: flex;
  }
  .pane.hidden {
    display: none;
  }
  .xterm-host {
    flex: 1;
    min-width: 0;
    min-height: 0;
    padding: 0.4rem 0 0 0.5rem;
  }
  .veil {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.7rem;
    background: rgba(20, 18, 31, 0.72);
    color: #b9b2d6;
    font-size: 0.86rem;
    text-align: center;
    padding: 1rem;
  }
  .veil.ended {
    background: rgba(20, 18, 31, 0.6);
  }
  .veil b {
    color: #e8e4f8;
  }
  .veil .diag {
    font-size: 0.68rem;
    color: #7d76a0;
  }
  .veil-actions {
    display: flex;
    gap: 0.5rem;
  }
  .btn-restart,
  .btn-close {
    border: 1px solid #494173;
    background: #242040;
    color: #d7d2ec;
    border-radius: 7px;
    padding: 0.34rem 0.7rem;
    font-size: 0.78rem;
    cursor: pointer;
  }
  .btn-restart:hover,
  .btn-close:hover {
    background: #322c55;
  }

  /* Last in the stylesheet on purpose: the phone-fullscreen
     overrides must out-cascade the base rules above (a later
     `padding` shorthand at equal specificity wipes the safe-area
     padding-top — exactly the under-the-notch bug). */
  @media (max-width: 700px) {
    .terminal {
      width: 100%;
      height: 100%;
      border: none;
      border-radius: 0;
    }
    .head {
      padding-top: calc(0.45rem + max(3.4rem, env(safe-area-inset-top, 0px)));
    }
  }
</style>
