<script lang="ts">
  // The finder-like file manager for one remote machine — its real disk on
  // the far side (the files host walks it with this user's permissions),
  // a simple list-and-preview surface on this side, and the mesh in
  // between. Two skins, one component: the desktop renders it `windowed`
  // (filling a dedicated OS window per machine); the web preview shows the
  // same thing as an in-page popover, where it's honest that live file
  // browsing needs the desktop app.
  //
  // One surface is one *session*: one route (minted by `filesConnect`),
  // request/response events riding it, every request tagged with a
  // viewer-minted id so a listing, a preview and an upload never tangle.
  // Downloads don't pass through here at all — `fileDownload` registers a
  // backend sink and the chunks stream straight into Downloads.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import {
    closeThisWindow,
    fileDownload,
    fileDownloadCancel,
    fileSend,
    isMobile,
    onFileProgress,
    onFileSaved,
    onThisWindowClose,
    watchFiles,
  } from "../tauri";
  import { FileRequests, uploadBrowserFile } from "../files-transfer";

  // Touch has no reliable double-tap (and iOS eats it for zoom), so folders
  // open on the second *single* tap instead: first tap selects (revealing the
  // row actions), tapping the selected row again opens it. No timing window.
  const mobile = isMobile();
  import { displayName, humanBytes, type FileEntry, type FileEvent } from "../types";

  let {
    host,
    windowed = false,
    mappedRoute = null,
    mappedLabel = "",
  }: { host: string; windowed?: boolean; mappedRoute?: string | null; mappedLabel?: string } = $props();

  const node = $derived(app.machineByAnyId(host));

  type Status = "connecting" | "live" | "rejected" | "ended" | "offline";
  let status = $state<Status>("connecting");
  let note = $state("");
  let routeId = $state<string | null>(null);
  let stopWatch: (() => void) | null = null;
  let started = false;

  // ---- the listing ----------------------------------------------------
  let path = $state("~");
  /** The host's home directory, from the first listing — the ⌂ anchor. */
  let home = $state("");
  let entries = $state<FileEntry[]>([]);
  let listing = $state(true); // a List request is in flight
  let showHidden = $state(false);
  let selected = $state<string | null>(null);

  const visible = $derived.by(() => {
    const list = showHidden ? entries : entries.filter((e) => !e.name.startsWith("."));
    return [...list].sort(
      (a, b) => Number(b.dir) - Number(a.dir) || a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
    );
  });

  // ---- request bookkeeping ---------------------------------------------
  let nextReq = 1;
  /** The id of the List currently in flight — stale listings are dropped. */
  let listReq = 0;
  /** Ops (mkdir/rename/delete) awaiting their ok/err. */
  const pendingOps = new Map<number, { label: string; refresh: boolean }>();
  const requests = new FileRequests((event) => {
    if (!routeId || status !== "live") return Promise.reject(new Error("The file session is not connected"));
    return fileSend(routeId, event);
  }, () => nextReq++);
  const uploads = new Map<number, AbortController>();
  const uploadTasks = new Set<Promise<void>>();
  /** Previews assembling from chunks. */
  const previews = new Map<number, { name: string; parts: Uint8Array[]; bytes: number; total: number }>();

  // ---- transfers (downloads + uploads) ----------------------------------
  interface Transfer {
    req: number;
    kind: "down" | "up";
    name: string;
    done: number;
    total: number;
    state: "moving" | "done" | "failed";
    note: string;
  }
  let transfers = $state<Transfer[]>([]);

  // ---- preview -----------------------------------------------------------
  /** Cap what we'll pull over the wire to look at (downloads are unbounded
   *  — they stream to disk backend-side; this is just the in-window peek). */
  const PREVIEW_MAX = 10 * 1024 * 1024;
  const TEXT_EXT = new Set([
    "txt", "md", "rs", "ts", "js", "tsx", "jsx", "svelte", "json", "toml", "yaml", "yml",
    "css", "html", "xml", "sh", "ps1", "py", "rb", "go", "c", "h", "cpp", "hpp", "java",
    "log", "ini", "cfg", "conf", "csv", "lock", "sql", "env", "gitignore",
  ]);
  const IMAGE_EXT: Record<string, string> = {
    png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
    webp: "image/webp", svg: "image/svg+xml", bmp: "image/bmp", ico: "image/x-icon",
    avif: "image/avif",
  };
  interface Preview {
    name: string;
    kind: "text" | "image" | "none";
    text: string;
    url: string; // blob URL for images
    loading: boolean;
    /** The host's reason when the read failed — shown in the pane. */
    error: string;
  }
  let preview = $state<Preview | null>(null);

  // ---- small helpers ----------------------------------------------------

  function childPath(name: string, parent = path): string {
    const separator = parent.includes("\\") ? "\\" : "/";
    return parent.endsWith(separator) ? parent + name : parent + separator + name;
  }

  function parentOf(p: string): string {
    const s = p.includes("\\") ? "\\" : "/";
    const trimmed = p.endsWith(s) && p.length > 1 ? p.slice(0, -1) : p;
    const i = trimmed.lastIndexOf(s);
    if (i < 0) return trimmed;
    const up = trimmed.slice(0, i);
    if (up === "") return s; // unix root
    if (/^[A-Za-z]:$/.test(up)) return up + s; // windows drive root
    return up;
  }

  const atRoot = $derived(path === parentOf(path));

  function extOf(name: string): string {
    const i = name.lastIndexOf(".");
    return i > 0 ? name.slice(i + 1).toLowerCase() : "";
  }

  function entryIcon(e: FileEntry): string {
    if (e.dir) return e.symlink ? "🔗" : "📁";
    const ext = extOf(e.name);
    if (ext in IMAGE_EXT) return "🖼";
    if (["mp4", "mov", "mkv", "webm", "avi"].includes(ext)) return "🎬";
    if (["mp3", "wav", "flac", "ogg", "m4a"].includes(ext)) return "🎵";
    if (["zip", "tar", "gz", "xz", "7z", "rar", "bz2"].includes(ext)) return "🗜";
    if (["pdf"].includes(ext)) return "📕";
    if (TEXT_EXT.has(ext)) return "📄";
    return "📄";
  }

  function whenLabel(secs?: number | null): string {
    if (!secs) return "";
    const d = new Date(secs * 1000);
    const now = Date.now();
    if (now - d.getTime() < 20 * 60 * 60 * 1000) {
      return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
    }
    return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  }

  function b64decode(text: string): Uint8Array {
    const bin = atob(text);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function send(event: FileEvent) {
    if (!routeId) return;
    void fileSend(routeId, event).catch((e) => {
      app.toast("warn", `Couldn't reach ${node?.label ?? "the machine"}: ${e}`);
    });
  }

  // ---- navigation --------------------------------------------------------

  /** Ask for a directory's contents. `path` itself only moves when the
   *  host's listing lands (the `entries` branch of `onEvent`) — never on
   *  the ask. A failed open must leave the path bar on the directory the
   *  list still shows, or one bad folder poisons every later attempt by
   *  compounding onto a path that was never valid. */
  function list(target: string) {
    const req = nextReq++;
    listReq = req;
    listing = true;
    selected = null;
    send({ kind: "list", req, path: target });
  }

  function open(e: FileEntry) {
    if (e.dir) {
      list(childPath(e.name));
    } else {
      openPreview(e);
    }
  }

  function goUp() {
    if (atRoot) return;
    list(parentOf(path));
  }

  function goHome() {
    list(home || "~");
  }

  function refresh() {
    list(path);
  }

  // ---- the response stream ------------------------------------------------

  function onEvent(ev: FileEvent) {
    if (requests.accept(ev)) return;
    switch (ev.kind) {
      case "entries": {
        if (ev.req !== listReq) return; // a stale listing
        path = ev.path;
        home = ev.home;
        entries = ev.entries;
        listing = false;
        return;
      }
      case "chunk": {
        const p = previews.get(ev.req);
        if (!p) return; // a download's chunk never reaches the window
        const bytes = b64decode(ev.data);
        p.parts.push(bytes);
        p.bytes += bytes.length;
        if (p.bytes > PREVIEW_MAX) {
          previews.delete(ev.req);
          if (preview?.loading) preview = { ...preview, kind: "none", loading: false };
          app.toast("info", "Too big to preview: download it instead");
          return;
        }
        if (ev.eof) {
          previews.delete(ev.req);
          finishPreview(p);
        }
        return;
      }
      case "ok": {
        const op = pendingOps.get(ev.req);
        if (!op) return;
        pendingOps.delete(ev.req);
        const t = transfers.find((x) => x.req === ev.req && x.kind === "up");
        if (t) {
          t.state = "done";
          t.done = t.total;
          t.note = "uploaded";
        }
        // No toast — the file list refreshes below (the new/renamed/removed
        // entry is the confirmation); uploads show "done" in the transfer row.
        if (op.refresh) refresh();
        return;
      }
      case "err": {
        if (ev.req === listReq) {
          listing = false;
          app.toast("warn", `Couldn't open that folder: ${ev.reason}`);
          return;
        }
        const p = previews.get(ev.req);
        if (p) {
          previews.delete(ev.req);
          // Tell the story in the pane itself (only if it's still about
          // this file) — and never touch the listing or the path.
          if (preview?.name === p.name) {
            preview = { name: p.name, kind: "none", text: "", url: "", loading: false, error: ev.reason };
          }
          return;
        }
        const op = pendingOps.get(ev.req);
        if (op) {
          pendingOps.delete(ev.req);
          const t = transfers.find((x) => x.req === ev.req && x.kind === "up");
          if (t) {
            t.state = "failed";
            t.note = ev.reason;
          }
          app.toast("warn", ev.reason);
        }
        return;
      }
      default:
        return; // requests echoing back would be a confused host
    }
  }

  // ---- preview -------------------------------------------------------------

  function openPreview(e: FileEntry) {
    // Already showing (or fetching) this very file — don't re-read it.
    // An errored attempt (kind "none" + error) does retry.
    if (preview?.name === e.name && (preview.loading || preview.kind !== "none" || !preview.error))
      return;
    const ext = extOf(e.name);
    const isImage = ext in IMAGE_EXT;
    const isText = TEXT_EXT.has(ext) || e.size <= 256 * 1024;
    if (preview?.url) URL.revokeObjectURL(preview.url);
    if (e.size > PREVIEW_MAX || (!isImage && !isText)) {
      preview = { name: e.name, kind: "none", text: "", url: "", loading: false, error: "" };
      return;
    }
    const req = nextReq++;
    previews.set(req, { name: e.name, parts: [], bytes: 0, total: e.size });
    preview = {
      name: e.name,
      kind: isImage ? "image" : "text",
      text: "",
      url: "",
      loading: true,
      error: "",
    };
    send({ kind: "read", req, path: childPath(e.name) });
  }

  function finishPreview(p: { name: string; parts: Uint8Array[]; bytes: number }) {
    if (!preview || preview.name !== p.name) return;
    const all = new Uint8Array(p.bytes);
    let off = 0;
    for (const part of p.parts) {
      all.set(part, off);
      off += part.length;
    }
    const ext = extOf(p.name);
    if (ext in IMAGE_EXT) {
      const url = URL.createObjectURL(new Blob([all.buffer as ArrayBuffer], { type: IMAGE_EXT[ext] }));
      preview = { name: p.name, kind: "image", text: "", url, loading: false, error: "" };
      return;
    }
    const text = new TextDecoder("utf-8", { fatal: false }).decode(all);
    // A pile of replacement chars means it wasn't text after all.
    const garbage = (text.slice(0, 2000).match(/�/g)?.length ?? 0) > 20;
    preview = garbage
      ? { name: p.name, kind: "none", text: "", url: "", loading: false, error: "" }
      : { name: p.name, kind: "text", text, url: "", loading: false, error: "" };
  }

  function closePreview() {
    if (preview?.url) URL.revokeObjectURL(preview.url);
    preview = null;
  }

  // ---- file management -------------------------------------------------------

  async function download(e: FileEntry) {
    if (!routeId || e.dir) return;
    const req = nextReq++;
    const rid = routeId;
    const source = childPath(e.name);
    // Register the disk sink *first*, then ask — the first chunk can't
    // race the registration.
    let registered = false;
    try {
      const dest = await fileDownload(rid, req, e.name);
      registered = true;
      if (closing || routeId !== rid || status !== "live") throw new Error("The file session ended");
      transfers.push({
          req,
          kind: "down",
          name: e.name,
          done: 0,
          total: e.size,
          state: "moving",
          note: dest,
      });
      await fileSend(rid, { kind: "read", req, path: source });
    } catch (error) {
      if (registered) await fileDownloadCancel(rid, req).catch(() => {});
      const transfer = transfers.find((item) => item.kind === "down" && item.req === req);
      if (transfer) { transfer.state = "failed"; transfer.note = String(error); }
      app.toast("warn", `Couldn't start the download: ${error}`);
    }
  }

  let uploadInput = $state<HTMLInputElement | null>(null);
  async function uploadFiles(files: FileList | null) {
    if (!files || !routeId) return;
    const destination = path;
    for (const file of Array.from(files)) {
      if (closing || status !== "live") break;
      const req = nextReq++;
      transfers.push({
        req,
        kind: "up",
        name: file.name,
        done: 0,
        total: file.size,
        state: "moving",
        note: "",
      });
      const transfer = transfers[transfers.length - 1];
      const controller = new AbortController();
      uploads.set(req, controller);
      const task = (async () => {
        try {
          await uploadBrowserFile(file, childPath(file.name, destination), requests, {
            signal: controller.signal,
            progress: (written) => { transfer.done = written; },
          });
          transfer.state = "done";
          transfer.done = transfer.total;
          transfer.note = "uploaded";
          if (!closing && status === "live") refresh();
        } catch (error) {
          transfer.state = "failed";
          transfer.note = String(error);
          if (!controller.signal.aborted) app.toast("warn", `Couldn't upload ${file.name}: ${error}`);
        } finally { uploads.delete(req); }
      })();
      uploadTasks.add(task);
      await task;
      uploadTasks.delete(task);
    }
    if (uploadInput) uploadInput.value = "";
  }

  let makingFolder = $state(false);
  let newFolderName = $state("");
  function makeFolder() {
    const name = newFolderName.trim();
    makingFolder = false;
    newFolderName = "";
    if (!name) return;
    const req = nextReq++;
    pendingOps.set(req, { label: `Made “${name}”`, refresh: true });
    send({ kind: "mkdir", req, path: childPath(name) });
  }

  let renaming = $state<string | null>(null); // the entry name being renamed
  let renameTo = $state("");
  function startRename(e: FileEntry) {
    renaming = e.name;
    renameTo = e.name;
  }
  function commitRename() {
    const from = renaming;
    const to = renameTo.trim();
    renaming = null;
    if (!from || !to || to === from) return;
    const req = nextReq++;
    pendingOps.set(req, { label: `Renamed to “${to}”`, refresh: true });
    send({ kind: "rename", req, from: childPath(from), to: childPath(to) });
  }

  /** Two-step delete: first click arms, second acts (the Fleet pattern). */
  let armedDelete = $state<string | null>(null);
  function removeEntry(e: FileEntry) {
    if (armedDelete !== e.name) {
      armedDelete = e.name;
      setTimeout(() => {
        if (armedDelete === e.name) armedDelete = null;
      }, 3500);
      return;
    }
    armedDelete = null;
    const req = nextReq++;
    pendingOps.set(req, { label: `Deleted ${e.name}`, refresh: true });
    send({ kind: "delete", req, path: childPath(e.name) });
  }

  function dismissTransfer(req: number) {
    transfers = transfers.filter((t) => t.req !== req);
  }

  // ---- session lifecycle -------------------------------------------------

  async function startSession() {
    if (started || !routeId) return;
    started = true;
    const rid = routeId;
    try {
      const stop = await watchFiles(rid, onEvent);
      if (closing || routeId !== rid) { stop(); return; }
      stopWatch = stop;
      list(path);
    } catch (error) {
      status = "ended";
      note = `Couldn't watch the file session: ${error}`;
      failTransfers(note);
    }
  }

  function failTransfers(reason: string) {
    for (const controller of uploads.values()) controller.abort(new Error(reason));
    requests.close(reason);
    for (const transfer of transfers) {
      if (transfer.state === "moving") { transfer.state = "failed"; transfer.note = reason; }
    }
  }

  $effect(() => {
    const states = app.routeStates;
    if (!routeId) return;
    const st = states[routeId];
    if (st?.state === "active") {
      if (status === "connecting") status = "live";
      if (status === "live" && !started) startSession();
    } else if (st?.state === "rejected") {
      if (status !== "rejected") {
        status = "rejected";
        note = st.reason || "the far side refused the session";
        failTransfers(note);
      }
    } else if (st?.state === "torn_down" && (status === "live" || status === "connecting")) {
      status = "ended";
      note = "session ended by the far side";
      failTransfers(note);
    }
  });

  let closing = false;
  let unlistenClose: (() => void) | null = null;
  let unlistenSaved: (() => void) | null = null;
  let unlistenProgress: (() => void) | null = null;

  /** Tear the session down and close the surface. Bounded — a wedged
   *  backend must never hold a closing window hostage. */
  async function endAll() {
    if (closing) return;
    closing = true;
    for (const controller of uploads.values()) controller.abort(new Error("Upload cancelled"));
    // Give acknowledged staging-file cleanup a bounded opportunity before
    // tearing down this window's temporary route.
    await Promise.race([Promise.allSettled([...uploadTasks]), new Promise((r) => setTimeout(r, 600))]);
    // A normal Files window owns its temporary route. A mapped drive does not:
    // closing the browser leaves the mapping alive until the user taps Unmap.
    const ownedRoute = routeId && !mappedRoute ? routeId : null;
    if (ownedRoute) routeId = null;
    const teardown = ownedRoute ? app.filesDisconnect(ownedRoute) : Promise.resolve();
    if (windowed) {
      await Promise.race([teardown, new Promise((r) => setTimeout(r, 600))]);
      void closeThisWindow();
    } else {
      void teardown;
      app.closeFiles();
    }
    closing = false;
  }

  onMount(() => {
    routeId = mappedRoute ?? app.filesConnect(host);
    if (!routeId) {
      status = "offline";
      note = "Live file browsing needs the desktop app.";
    }
    // Session events are live truth; one pull closes the missed-event window.
    // There is deliberately no recurring IPC/network poll for route state.
    void app.refreshSession();
    void onFileSaved((ev) => {
      if (ev.route !== routeId) return;
      const t = transfers.find((x) => x.req === ev.req && x.kind === "down");
      if (!t) return;
      if (ev.error) {
        t.state = "failed";
        t.note = ev.error;
        app.toast("warn", `Download failed: ${ev.error}`);
      } else {
        t.state = "done";
        t.done = t.total;
        t.note = ev.path ?? "";
        app.toast("ok", `Saved ${t.name} to Downloads`);
      }
    }).then((u) => (unlistenSaved = u));
    void onFileProgress((ev) => {
      if (ev.route !== routeId) return;
      const t = transfers.find((x) => x.req === ev.req && x.kind === "down");
      if (t) {
        t.done = ev.written;
        if (ev.total > 0) t.total = ev.total;
      }
    }).then((u) => (unlistenProgress = u));
    if (windowed) {
      void onThisWindowClose(() => void endAll()).then((u) => (unlistenClose = u));
    }
    return () => {
      failTransfers("The file window closed");
      stopWatch?.();
      unlistenSaved?.();
      unlistenProgress?.();
      unlistenClose?.();
      if (preview?.url) URL.revokeObjectURL(preview.url);
      // Window-close calls endAll first. A direct component remount does not,
      // so always retire this window's temporary browsing route on unmount.
      // Persistent mapped-drive routes remain owned by the mapping itself.
      if (routeId && !mappedRoute) void app.filesDisconnect(routeId);
    };
  });

  const moving = $derived(transfers.filter((t) => t.state === "moving"));
</script>

{#if node}
  <div class="scrim" class:windowed>
    {#if !windowed}
      <button class="backdrop" aria-label="Close files" onclick={() => void endAll()}></button>
    {/if}
    <div class="files" role="dialog" aria-modal={!windowed} aria-label="Files on {displayName(node)}">
      <header class="head">
        <div class="who">
          <span class="ico">🗂</span>
          <div class="meta">
            <div class="name">{mappedLabel || displayName(node)}</div>
            <div class="sub">
              <span class="dot" class:on={node.online}></span>
              {node.online ? "online" : "offline"} · files
            </div>
          </div>
        </div>
        <div class="nav">
          <button class="btn small" onclick={goUp} disabled={atRoot || status !== "live"} title="Up one folder">↑ Up</button>
          <button class="btn small" onclick={goHome} disabled={status !== "live"} title="The machine's home folder">⌂ Home</button>
          <div class="pathbar" title={path}>{path}</div>
          <button class="btn small" onclick={refresh} disabled={status !== "live"} title="Refresh">↻</button>
        </div>
        <button class="x" onclick={() => void endAll()} aria-label="Close">✕</button>
      </header>

      <div class="toolbar">
        <button class="btn small" disabled={status !== "live"} onclick={() => (makingFolder = true)}>＋ New folder</button>
        <button class="btn small" disabled={status !== "live"} onclick={() => uploadInput?.click()}>⬆ Upload here</button>
        <input
          class="file-input"
          type="file"
          multiple
          bind:this={uploadInput}
          onchange={(e) => void uploadFiles(e.currentTarget.files)}
        />
        <label class="hidden-toggle">
          <input type="checkbox" bind:checked={showHidden} />
          <span>Hidden files</span>
        </label>
      </div>

      <div class="stage">
        {#if status === "live"}
          <div class="list" role="listbox" aria-label="Files in {path}">
            <div class="row header" aria-hidden="true">
              <span class="c-icon"></span>
              <span class="c-name">Name</span>
              <span class="c-size">Size</span>
              <span class="c-when">Modified</span>
              <span class="c-acts"></span>
            </div>
            {#if makingFolder}
              <div class="row editing">
                <span class="c-icon">📁</span>
                <!-- svelte-ignore a11y_autofocus -->
                <input
                  class="rename-input c-name"
                  placeholder="Folder name"
                  autofocus
                  bind:value={newFolderName}
                  onkeydown={(e) => {
                    if (e.key === "Enter") makeFolder();
                    if (e.key === "Escape") { makingFolder = false; newFolderName = ""; }
                  }}
                  onblur={makeFolder}
                />
              </div>
            {/if}
            {#each visible as e (e.name)}
              <div
                class="row"
                class:selected={selected === e.name}
                role="option"
                aria-selected={selected === e.name}
                tabindex="0"
                onclick={() => {
                  const was = selected === e.name;
                  // On touch, a second tap on the selected row opens it —
                  // the double-click with the timing window removed.
                  if (mobile && was) {
                    open(e);
                    return;
                  }
                  selected = was ? null : e.name;
                  // Selecting a file previews it right away (the finder
                  // habit) — folders still wait for the double-click.
                  if (!e.dir && !was) openPreview(e);
                }}
                ondblclick={() => open(e)}
                onkeydown={(ev) => {
                  if (ev.key === "Enter") open(e);
                }}
              >
                <span class="c-icon">{entryIcon(e)}</span>
                {#if renaming === e.name}
                  <!-- svelte-ignore a11y_autofocus -->
                  <input
                    class="rename-input c-name"
                    autofocus
                    bind:value={renameTo}
                    onclick={(ev) => ev.stopPropagation()}
                    onkeydown={(ev) => {
                      ev.stopPropagation();
                      if (ev.key === "Enter") commitRename();
                      if (ev.key === "Escape") renaming = null;
                    }}
                    onblur={commitRename}
                  />
                {:else}
                  <span class="c-name" title={e.name}>
                    {e.name}{#if e.symlink}<span class="link-mark" title="symlink"> ⤳</span>{/if}
                  </span>
                {/if}
                <span class="c-size">{e.dir ? "Folder" : humanBytes(e.size)}</span>
                <span class="c-when">{whenLabel(e.modified)}</span>
                <span class="c-acts">
                  {#if !e.dir}
                    <button class="act" title="Save to this machine's Downloads" onclick={(ev) => { ev.stopPropagation(); download(e); }}>⬇</button>
                  {/if}
                  <button class="act" title="Rename" onclick={(ev) => { ev.stopPropagation(); startRename(e); }}>✎</button>
                  <button
                    class="act danger"
                    class:armed={armedDelete === e.name}
                    title="Delete"
                    onclick={(ev) => { ev.stopPropagation(); removeEntry(e); }}
                  >{armedDelete === e.name ? "sure?" : "✕"}</button>
                </span>
              </div>
            {/each}
            {#if visible.length === 0 && !listing && !makingFolder}
              <div class="empty">Nothing {showHidden ? "here" : "visible here"}.</div>
            {/if}
            {#if listing}
              <div class="empty">Loading…</div>
            {/if}
          </div>

          {#if preview}
            <div class="preview">
              <header class="p-head">
                <span class="p-name" title={preview.name}>{preview.name}</span>
                <button class="x small-x" onclick={closePreview} aria-label="Close preview">✕</button>
              </header>
              {#if preview.loading}
                <div class="p-body p-center">Loading preview…</div>
              {:else if preview.kind === "image"}
                <div class="p-body p-center"><img src={preview.url} alt={preview.name} /></div>
              {:else if preview.kind === "text"}
                <pre class="p-body">{preview.text}</pre>
              {:else if preview.error}
                <div class="p-body p-center muted">
                  Couldn't read this file: {preview.error}
                </div>
              {:else}
                <div class="p-body p-center muted">
                  No preview for this file: use ⬇ to save it to Downloads.
                </div>
              {/if}
            </div>
          {/if}
        {:else}
          <div class="veil">
            {#if status === "connecting"}
              <p>Connecting to <b>{displayName(node)}</b>…</p>
              <p class="diag">
                route {app.routeStates[routeId ?? ""]?.state ?? "not negotiated yet"}
              </p>
            {:else if status === "rejected"}
              <p>Refused: {note}</p>
            {:else}
              <p>{note}</p>
            {/if}
          </div>
        {/if}
      </div>

      {#if transfers.length}
        <footer class="transfers">
          {#each transfers as t (t.kind + t.req)}
            <div class="transfer" class:failed={t.state === "failed"}>
              <span class="t-dir">{t.kind === "down" ? "⬇" : "⬆"}</span>
              <span class="t-name" title={t.note || t.name}>{t.name}</span>
              {#if t.state === "moving"}
                <progress max={t.total || 1} value={t.total ? t.done : undefined}></progress>
                <span class="t-note">{humanBytes(t.done)}{t.total ? ` / ${humanBytes(t.total)}` : ""}</span>
                {#if t.kind === "up"}
                  <button class="x small-x" onclick={() => uploads.get(t.req)?.abort(new Error("Upload cancelled"))} aria-label="Cancel upload">✕</button>
                {/if}
              {:else if t.state === "done"}
                <span class="t-note ok" title={t.note}>{t.kind === "down" ? "saved to Downloads" : "uploaded"} ✓</span>
                <button class="act" title="Dismiss" onclick={() => dismissTransfer(t.req)}>✕</button>
              {:else}
                <span class="t-note bad" title={t.note}>failed: {t.note}</span>
                <button class="act" title="Dismiss" onclick={() => dismissTransfer(t.req)}>✕</button>
              {/if}
            </div>
          {/each}
          {#if moving.length === 0}
            <button class="linklike clear" onclick={() => (transfers = [])}>Clear finished</button>
          {/if}
        </footer>
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    display: grid;
    place-items: center;
    z-index: 60;
  }
  .scrim:not(.windowed) {
    background: rgba(20, 18, 31, 0.45);
  }
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .files {
    position: relative;
    z-index: 1;
    display: flex;
    flex-direction: column;
    width: 56rem;
    max-width: 94vw;
    height: 36rem;
    max-height: 90vh;
    background: var(--surface);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
    animation: rise 0.16s ease;
  }
  .windowed .files {
    width: 100vw;
    max-width: 100vw;
    height: 100vh;
    max-height: 100vh;
    border-radius: 0;
    box-shadow: none;
  }
  /* Phone-width: the in-page file manager takes the whole screen (the scrim
     is already above the header + portrait pill dock at z 60). Safe-area
     padding keeps the header's ✕ out from under the status bar. Zero effect
     on desktop: env() is 0 there, and a narrow *windowed* files view is
     already full-size. */
  @keyframes rise {
    from {
      transform: translateY(12px) scale(0.98);
      opacity: 0;
    }
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.8rem;
    padding: 0.6rem 0.8rem;
    border-bottom: 1px solid var(--line);
    flex-shrink: 0;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    min-width: 0;
  }
  .ico {
    font-size: 1.3rem;
  }
  .meta .name {
    font-weight: 700;
    font-size: 0.92rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 14rem;
  }
  .meta .sub {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.7rem;
    color: var(--ink-faint);
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--line-strong);
  }
  .dot.on {
    background: var(--ok);
  }
  .nav {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex: 1;
    min-width: 0;
  }
  .pathbar {
    flex: 1;
    min-width: 0;
    font-size: 0.78rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    color: var(--ink-soft);
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.35rem 0.6rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    direction: rtl; /* the tail of a long path is the part that matters */
    text-align: left;
  }
  .x {
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    font-size: 0.8rem;
    flex-shrink: 0;
  }
  .x:hover {
    background: var(--line-strong);
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 0.8rem;
    border-bottom: 1px solid var(--line);
    flex-shrink: 0;
  }
  .file-input {
    display: none;
  }
  .hidden-toggle {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.74rem;
    color: var(--ink-soft);
    cursor: pointer;
  }
  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .list {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 0.3rem 0.4rem 0.8rem;
  }
  .row {
    display: grid;
    grid-template-columns: 2rem 1fr 6rem 8rem 6.5rem;
    align-items: center;
    gap: 0.4rem;
    padding: 0.32rem 0.5rem;
    border-radius: var(--r-sm);
    font-size: 0.84rem;
    cursor: default;
    user-select: none;
  }
  .row.header {
    position: sticky;
    top: 0;
    background: var(--surface);
    font-size: 0.66rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
    font-weight: 700;
    z-index: 1;
  }
  .row:not(.header):hover {
    background: var(--surface-2);
  }
  .row.selected {
    background: var(--accent-soft);
  }
  .c-icon {
    text-align: center;
  }
  .c-name {
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-weight: 550;
  }
  .link-mark {
    color: var(--ink-faint);
  }
  .c-size,
  .c-when {
    font-size: 0.74rem;
    color: var(--ink-faint);
    text-align: right;
    white-space: nowrap;
  }
  .c-acts {
    display: flex;
    justify-content: flex-end;
    gap: 0.15rem;
    visibility: hidden;
  }
  .row:hover .c-acts,
  .row.selected .c-acts {
    visibility: visible;
  }
  .act {
    border: none;
    background: transparent;
    color: var(--ink-soft);
    width: 1.6rem;
    height: 1.6rem;
    border-radius: var(--r-sm);
    font-size: 0.78rem;
  }
  .act:hover {
    background: var(--line);
  }
  .act.danger:hover,
  .act.danger.armed {
    background: var(--danger-soft);
    color: var(--danger);
    width: auto;
    padding: 0 0.4rem;
  }
  .rename-input {
    border: 1px solid var(--accent);
    border-radius: var(--r-sm);
    padding: 0.15rem 0.4rem;
    font-size: 0.84rem;
    background: var(--surface);
    color: var(--ink);
  }
  .empty {
    padding: 2rem 1rem;
    text-align: center;
    color: var(--ink-faint);
    font-size: 0.84rem;
  }
  .preview {
    width: 46%;
    max-width: 30rem;
    border-left: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    min-width: 0;
    background: var(--surface);
  }
  .p-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 0.6rem;
    border-bottom: 1px solid var(--line);
  }
  .p-name {
    flex: 1;
    min-width: 0;
    font-size: 0.8rem;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .small-x {
    width: 1.5rem;
    height: 1.5rem;
    font-size: 0.7rem;
  }
  .p-body {
    flex: 1;
    min-height: 0;
    overflow: auto;
    margin: 0;
    padding: 0.6rem 0.7rem;
    font-size: 0.76rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .p-center {
    display: grid;
    place-items: center;
    color: var(--ink-soft);
    font-family: inherit;
  }
  .p-center img {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    border-radius: var(--r-sm);
  }
  .muted {
    color: var(--ink-faint);
  }
  .veil {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.4rem;
    color: var(--ink-soft);
    font-size: 0.9rem;
    text-align: center;
    padding: 2rem;
  }
  .diag {
    font-size: 0.7rem;
    color: var(--ink-faint);
  }
  .transfers {
    border-top: 1px solid var(--line);
    padding: 0.4rem 0.8rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    max-height: 9rem;
    overflow-y: auto;
    flex-shrink: 0;
  }
  .transfer {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.78rem;
  }
  .transfer.failed {
    color: var(--danger);
  }
  .t-name {
    min-width: 0;
    max-width: 16rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-weight: 600;
  }
  .transfer progress {
    flex: 1;
    height: 0.5rem;
  }
  .t-note {
    color: var(--ink-faint);
    white-space: nowrap;
  }
  .t-note.ok {
    color: var(--ok);
  }
  .t-note.bad {
    color: var(--danger);
    max-width: 18rem;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .linklike.clear {
    align-self: flex-end;
    border: none;
    background: none;
    color: var(--accent-ink);
    font-size: 0.72rem;
    cursor: pointer;
  }

  /* Last in the stylesheet on purpose: the phone-fullscreen
     overrides must out-cascade the base rules above (a later
     `padding` shorthand at equal specificity wipes the safe-area
     padding-top — exactly the under-the-notch bug). */
  @media (max-width: 700px) {
    .files {
      width: 100%;
      max-width: 100%;
      height: 100%;
      max-height: 100%;
      border-radius: 0;
    }
    .head {
      padding-top: calc(0.6rem + max(3.4rem, env(safe-area-inset-top, 0px)));
    }
  }

  /* Touch: the hover-reveal row actions are finger targets here, not mouse
     targets — give them the ~40px minimum and a little air. (Reveal itself
     already works on touch: tapping a row selects it, and `.row.selected
     .c-acts` is visible.) */
  @media (hover: none) {
    .row {
      min-height: 2.75rem;
    }
    .act {
      width: 2.5rem;
      height: 2.5rem;
      font-size: 0.95rem;
    }
  }
</style>
