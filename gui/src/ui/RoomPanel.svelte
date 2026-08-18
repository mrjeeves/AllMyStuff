<script lang="ts">
  // The call panel of a virtual room — what "joining" opens. It reads
  // like any call app you've used: a dark stage with a tile per person
  // present (screen shares take the stage, people ride a filmstrip), a
  // centred control strip at the bottom where **everything starts off**,
  // chat and people panels on the right, and Leave in red at the far
  // right. On the desktop the whole panel lives in its own OS window
  // (full-screenable, movable); the web preview keeps the in-page
  // overlay. Each toggle fans ordinary routes out to the members, so
  // authorization applies the same as wiring on the graph.
  //
  // The strip keeps two audio ideas deliberately apart:
  //   🎙 Mic — your voice to the room (the call itself);
  //   🔊 Share sound — what this *machine* is playing (its loopback),
  //      never your microphone.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { clipboardWrite, toggleWindowFullscreen } from "../tauri";
  import { humanBytes, type MeshNode } from "../types";
  import RoomTile from "./RoomTile.svelte";

  let { windowed = false }: { windowed?: boolean } = $props();

  onMount(() => {
    // Ask the popout lane who's already out there, so tiles whose stream
    // lives in another window show "Return video here" from the start.
    app.helloVideoLane();
  });

  const room = $derived(app.openRoom);
  let chatInput = $state("");
  let chatLog = $state<HTMLDivElement | null>(null);
  // The screen-selection popover (like Zoom's): shown when there's more
  // than one monitor to choose between.
  let screenPickerOpen = $state(false);
  // The owner's inline rename: the title flips to an input in place.
  let renaming = $state(false);
  let renameDraft = $state("");
  // "Copy invite" flips its label for a beat after copying.
  let copied = $state(false);
  // OS-window fullscreen (room windows only — the web overlay has no
  // window of its own to fill).
  let fullscreen = $state(false);

  function startRename() {
    if (!room || !app.canRenameRoom(room)) return;
    renameDraft = room.name;
    renaming = true;
  }

  function commitRename() {
    if (!renaming) return;
    renaming = false;
    if (room) app.renameRoom(room.id, renameDraft);
  }

  const chat = $derived(room ? app.roomChat[room.id] ?? [] : []);
  const unread = $derived(room ? app.roomUnread[room.id] ?? 0 : 0);
  const knocks = $derived(room && app.isRoomHost(room) ? app.roomKnocks[room.id] ?? [] : []);
  // The room's Shared Files — yours (with a ✕ to stop sharing) and other
  // members' (with a Download). The host hosts this list; bytes come
  // straight from each uploader.
  const sharedFiles = $derived(app.roomSharedFiles);

  const inviteCandidates = $derived(
    room
      ? app.roomCandidateNodes.filter(
          (n) => !room.members.some((m) => app.machineByAnyId(m)?.id === n.id),
        )
      : [],
  );

  function inRoom(memberId: string): boolean {
    if (!room) return false;
    return (app.roomPresence[room.id] ?? []).some((m) => m === memberId);
  }

  // ---- who's on the stage ---------------------------------------------
  //
  // A tile per person *in the call* (presence), you included — exactly
  // what every call app trains people to expect. Members who haven't
  // joined yet live in the People panel, not on the stage.

  interface Participant {
    id: string;
    who: string;
    machine: string | null;
    me: boolean;
    online: boolean;
    /** Sending into the call right now (their mic / your mic toggle). */
    mic: boolean;
    /** Sharing machine sound into the call. */
    sound: boolean;
    /** You only: the other send toggles, for the self tile's badges. */
    screen: boolean;
    cam: boolean;
    control: boolean;
  }

  const presentPeople = $derived.by((): Participant[] => {
    if (!room) return [];
    const meWho = app.roomWho(app.localId);
    const out: Participant[] = [
      {
        id: app.localId,
        who: meWho.who,
        machine: meWho.machine,
        me: true,
        online: true,
        mic: app.roomMic,
        sound: app.roomSound,
        screen: app.roomScreen,
        cam: app.roomCam,
        control: app.roomControl,
      },
    ];
    for (const m of app.roomMemberNodes) {
      if (!inRoom(m.id)) continue;
      const w = app.roomWho(m.id);
      const sends = m.node ? app.roomMemberSends(m.node.id) : { mic: false, sound: false };
      out.push({
        id: m.id,
        who: w.who,
        machine: w.machine,
        me: false,
        online: !!m.node?.online,
        mic: sends.mic,
        sound: sends.sound,
        screen: false,
        cam: false,
        control: false,
      });
    }
    return out;
  });

  const awayMembers = $derived(room ? app.roomMemberNodes.filter((m) => !inRoom(m.id)) : []);

  function initials(who: string): string {
    const words = who.replace(/\(.*?\)/g, "").trim().split(/[\s·]+/).filter(Boolean);
    const a = words[0]?.[0] ?? "?";
    const b = words.length > 1 ? words[1][0] : "";
    return (a + b).toUpperCase();
  }

  function memberNote(n: MeshNode | undefined): string | null {
    if (!n) return null;
    if (!app.roomsSupported(n)) return "runs an older AllMyStuff — they won't see invites or chat";
    return null;
  }

  // ---- the call timer ---------------------------------------------------

  let now = $state(Date.now());
  $effect(() => {
    if (!room) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const callFor = $derived.by(() => {
    const at = room ? app.roomJoinedAt[room.id] : undefined;
    if (!at) return null;
    const s = Math.max(0, Math.floor((now - at) / 1000));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    const mm = h > 0 ? String(m).padStart(2, "0") : String(m);
    return `${h > 0 ? `${h}:` : ""}${mm}:${String(sec).padStart(2, "0")}`;
  });

  // ---- panel verbs -------------------------------------------------------

  function sendChat() {
    if (!chatInput.trim()) return;
    app.sendRoomChat(chatInput);
    chatInput = "";
  }

  function toggleChat() {
    app.roomChatOpen = !app.roomChatOpen;
    if (app.roomChatOpen) {
      app.roomPeopleOpen = false;
      app.roomFilesOpen = false;
    }
    if (app.roomChatOpen && room) app.roomUnread = { ...app.roomUnread, [room.id]: 0 };
  }

  function togglePeople() {
    app.roomPeopleOpen = !app.roomPeopleOpen;
    if (app.roomPeopleOpen) {
      app.roomChatOpen = false;
      app.roomFilesOpen = false;
    }
  }

  function toggleFiles() {
    app.roomFilesOpen = !app.roomFilesOpen;
    if (app.roomFilesOpen) {
      app.roomChatOpen = false;
      app.roomPeopleOpen = false;
    }
  }

  // New chat while the sidebar is open is read immediately; keep the log
  // pinned to the newest line. (The unread write is guarded — an effect
  // that unconditionally writes what it reads never settles.)
  $effect(() => {
    void chat.length;
    if (app.roomChatOpen && room) {
      if ((app.roomUnread[room.id] ?? 0) !== 0) {
        app.roomUnread = { ...app.roomUnread, [room.id]: 0 };
      }
      const el = chatLog;
      if (el) requestAnimationFrame(() => (el.scrollTop = el.scrollHeight));
    }
  });

  // "Share screen": one monitor shares straight away; several pop the
  // selection menu so you pick which (the way every call app does).
  function shareScreen() {
    if (app.roomScreen) {
      app.toggleRoomScreen();
      return;
    }
    if (app.roomScreenSources.length > 1) {
      screenPickerOpen = !screenPickerOpen;
      return;
    }
    app.toggleRoomScreen();
  }

  function pickScreen(id: string) {
    screenPickerOpen = false;
    app.toggleRoomScreen(id);
  }

  // "Share files": add files to the room's Shared Files area, then open it
  // so you see them land. (A call shares files for download — it's not the
  // file manager: no browsing or editing anyone's disk.)
  async function shareFiles() {
    app.roomFilesOpen = true;
    app.roomChatOpen = false;
    app.roomPeopleOpen = false;
    await app.shareRoomFiles();
  }

  async function copyInvite() {
    if (!room) return;
    try {
      await clipboardWrite(room.id);
      copied = true;
      setTimeout(() => (copied = false), 1600);
    } catch {
      /* clipboard unavailable — the id is visible in the tooltip */
    }
  }

  async function flipFullscreen() {
    fullscreen = await toggleWindowFullscreen();
  }

  function timeOf(at: number): string {
    return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }

  // The call-app staples: M mutes/unmutes, V flips the camera — unless
  // you're typing (chat, rename) or driving a shared screen (the tile
  // owns every key while control is granted).
  function onShortcut(e: KeyboardEvent) {
    if (!room || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target as HTMLElement | null;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    if (t?.closest('[role="application"]')) return;
    if (e.key === "m" || e.key === "M") {
      e.preventDefault();
      app.toggleRoomMic();
    } else if (e.key === "v" || e.key === "V") {
      e.preventDefault();
      app.toggleRoomCam();
    }
  }
</script>

<svelte:window onkeydown={onShortcut} />

{#snippet personTile(p: Participant, compact: boolean)}
  <div class="person" class:compact class:speaking={p.mic} class:offline={!p.me && !p.online}>
    <div class="avatar" aria-hidden="true">{initials(p.who)}</div>
    {#if p.me && (p.screen || p.sound || p.control || p.cam)}
      <div class="sending">
        {#if p.screen}<span class="send-chip ok">🖥 sharing screen</span>{/if}
        {#if p.sound}<span class="send-chip ok">🔊 sharing sound</span>{/if}
        {#if p.control}<span class="send-chip ok">🕹 control open</span>{/if}
        {#if p.cam}<span class="send-chip ok">📷 camera live</span>{/if}
      </div>
    {/if}
    <div class="nameplate">
      {#if p.mic}
        <span class="mic-state live" title={p.me ? "Your mic is live" : "Talking (their mic is live)"}>🎙</span>
      {:else}
        <span class="mic-state muted" title={p.me ? "You're muted" : "Their mic is off"}>🎙</span>
      {/if}
      <span class="plate-who">{p.who}</span>
      {#if p.machine}<span class="plate-machine">· {p.machine}</span>{/if}
      {#if !p.me && p.sound}<span title="Sharing their machine's sound">🔊</span>{/if}
    </div>
  </div>
{/snippet}

{#if room}
  <div class="room-wrap" class:windowed>
    <section class="room" aria-label="Room {room.name}">
      <header class="head">
        {#if renaming}
          <!-- svelte-ignore a11y_autofocus — the input replaces the title
               the user just clicked; focusing it is the whole point. -->
          <input
            class="title-input"
            autofocus
            bind:value={renameDraft}
            onkeydown={(e) => {
              if (e.key === "Enter") commitRename();
              if (e.key === "Escape") renaming = false;
            }}
            onblur={commitRename}
          />
        {:else if app.canRenameRoom(room)}
          <button class="title title-btn" title="Rename this room (you made it)" onclick={startRename}>
            🪩 {room.name} <span class="pencil" aria-hidden="true">✎</span>
          </button>
        {:else}
          <div class="title">🪩 {room.name}</div>
        {/if}
        <span
          class="host-chip"
          class:mine={app.isRoomHost(room)}
          title="The room's identity, roster and name live with its host. Rooms are stream-only — nothing is stored, here or anywhere — and sharing in one is scoped to the room: it never adds standing permissions."
        >
          🏠 {app.isRoomHost(room) ? "you host" : app.roomHostLabel(room)}
        </span>
        {#if app.isRoomHost(room)}
          <button
            class="access-chip"
            class:open={app.roomAccess(room) === "open"}
            title={app.roomAccess(room) === "open"
              ? "Open room: anyone you give its id can join straight in. Click to make it invite-only."
              : "Invite-only: a pasted id knocks, and you admit each ask. Click to open the room up."}
            onclick={() => app.setRoomAccess(room.id, app.roomAccess(room) === "open" ? "invite" : "open")}
          >
            {app.roomAccess(room) === "open" ? "🔓 open" : "🔒 invite-only"}
          </button>
        {:else if app.roomAccess(room) === "open"}
          <span class="access-chip open" title="Anyone with this room's id can join — copy the invite to pass it on">🔓 open</span>
        {/if}
        {#if callFor}
          <span class="timer" title="How long you've been in this call">{callFor}</span>
        {/if}
        <div class="head-spacer"></div>
        <button
          class="btn small"
          title="Copy this room's id — anyone on a shared network pastes it under Rooms → Join with an id ({room.id})"
          onclick={copyInvite}
        >
          {copied ? "✓ Copied" : "🔗 Copy invite"}
        </button>
        {#if windowed}
          <button
            class="btn small ghost"
            title={fullscreen ? "Leave fullscreen" : "Fullscreen"}
            aria-label={fullscreen ? "Leave fullscreen" : "Fullscreen"}
            onclick={flipFullscreen}>⛶</button
          >
        {:else}
          <button
            class="btn small ghost mini"
            title="Back to the graph — you stay in the room (everything keeps streaming)"
            aria-label="Minimize the room"
            onclick={() => app.closeRoomPanel()}>—</button
          >
        {/if}
      </header>

      {#if app.roomScreen}
        <div class="share-banner">
          <span class="share-dot" aria-hidden="true"></span>
          You're sharing your screen with the room
          <button class="stop-share" onclick={() => app.toggleRoomScreen()}>Stop share</button>
        </div>
      {/if}

      <div class="body">
        <div class="stage">
          {#if app.roomInboundShares.length > 0}
            <div class="share-stage" class:single={app.roomInboundShares.length === 1}>
              {#each app.roomInboundShares as share (share.route.id)}
                <RoomTile route={share.route} member={share.member} {windowed} />
              {/each}
            </div>
            <div class="filmstrip">
              {#each presentPeople as p (p.id)}
                {@render personTile(p, true)}
              {/each}
            </div>
          {:else}
            <div class="gallery" class:few={presentPeople.length <= 2}>
              {#each presentPeople as p (p.id)}
                {@render personTile(p, false)}
              {/each}
            </div>
          {/if}
        </div>

        {#if app.roomChatOpen}
          <aside class="side chat">
            <header class="side-head">
              <h4>Chat</h4>
              <button class="side-x" aria-label="Close chat" onclick={toggleChat}>✕</button>
            </header>
            <div class="chat-log" bind:this={chatLog}>
              {#each chat as line, i (i)}
                {@const by = app.roomChatWho(line)}
                <div class="line" class:mine={app.isMe(line.from)}>
                  <span class="line-who">{by.who}</span>
                  {#if by.machine}<span class="line-machine">· {by.machine}</span>{/if}
                  <span class="line-at">{timeOf(line.at)}</span>
                  <div class="line-text">{line.text}</div>
                </div>
              {:else}
                <p class="chat-empty">
                  No messages yet.{#if !app.backendConnected}&nbsp;(Demo mode — chat stays on this
                    device.){/if}
                </p>
              {/each}
            </div>
            <div class="chat-send">
              <input
                placeholder="Message the room…"
                bind:value={chatInput}
                onkeydown={(e) => e.key === "Enter" && sendChat()}
              />
              <button class="btn small primary" onclick={sendChat}>Send</button>
            </div>
          </aside>
        {/if}

        {#if app.roomPeopleOpen}
          <aside class="side people">
            <header class="side-head">
              <h4>People · {presentPeople.length} in the call</h4>
              <button class="side-x" aria-label="Close people" onclick={togglePeople}>✕</button>
            </header>
            <div class="people-scroll">
              {#if knocks.length > 0}
                <div class="knocks">
                  <h5>Asking to join</h5>
                  {#each knocks as k (k.from)}
                    <div class="knock-row">
                      <span class="knock-who">{app.roomWho(k.from).who}</span>
                      <button class="btn small primary" onclick={() => app.admitKnock(room.id, k.from)}>Admit</button>
                      <button class="btn small" onclick={() => app.denyKnock(room.id, k.from)}>Deny</button>
                    </div>
                  {/each}
                </div>
              {/if}
              <ul class="roster">
                {#each presentPeople as p (p.id)}
                  <li>
                    <span class="m-dot on"></span>
                    <span class="r-who">{p.who}</span>
                    {#if p.machine}<span class="r-machine">· {p.machine}</span>{/if}
                    {#if p.mic}<span title="Talking">🎙</span>{/if}
                    {#if p.sound}<span title="Sharing sound">🔊</span>{/if}
                    {#if p.me && p.screen}<span title="Sharing your screen">🖥</span>{/if}
                    {#if p.me && p.cam}<span title="Sharing your camera">📷</span>{/if}
                    {#if !p.me && app.isRoomHost(room)}
                      <button
                        class="m-remove"
                        title="Remove this machine from the room"
                        aria-label="Remove member"
                        onclick={() => app.removeRoomMember(room.id, p.id)}>✕</button
                      >
                    {/if}
                  </li>
                {/each}
                {#each awayMembers as m (m.id)}
                  {@const w = app.roomWho(m.id)}
                  {@const note = memberNote(m.node)}
                  <li class="away">
                    <span class="m-dot" class:on={!!m.node?.online}></span>
                    <span class="r-who">{w.who}</span>
                    {#if w.machine}<span class="r-machine">· {w.machine}</span>{/if}
                    <span class="r-state" title={note ?? undefined}>
                      {#if note}⚠ {!app.roomsSupported(m.node) ? "old app" : "unclaimed"}{:else if m.node?.online}invited — not in yet{:else}offline{/if}
                    </span>
                    {#if app.isRoomHost(room)}
                      <button
                        class="m-remove"
                        title="Remove this machine from the room"
                        aria-label="Remove member"
                        onclick={() => app.removeRoomMember(room.id, m.id)}>✕</button
                      >
                    {/if}
                  </li>
                {/each}
              </ul>
              {#if app.isRoomHost(room)}
                <div class="invite-block">
                  <h5>Invite a machine</h5>
                  {#each inviteCandidates as n (n.id)}
                    {@const w = app.roomWho(n.id)}
                    <button class="invite-row" onclick={() => app.addRoomMembers(room.id, [n.id])}>
                      <span class="m-dot" class:on={n.online}></span>
                      {w.who}{#if w.machine}&nbsp;<span class="r-machine">· {w.machine}</span>{/if}
                    </button>
                  {:else}
                    <p class="fine">Every machine on the graph is already in this room.</p>
                  {/each}
                </div>
              {/if}
            </div>
          </aside>
        {/if}

        {#if app.roomFilesOpen}
          <aside class="side files-side">
            <header class="side-head">
              <h4>Shared Files · {sharedFiles.length}</h4>
              <button class="side-x" aria-label="Close shared files" onclick={toggleFiles}>✕</button>
            </header>
            <div class="files-scroll">
              <button class="share-add" onclick={shareFiles}>＋ Share a file with the room</button>
              {#each sharedFiles as s (s.from + s.file.token)}
                {@const dl = app.sharedDownloads[s.file.token]}
                <div class="shared-row" class:mine={s.me}>
                  <div class="shared-icon" aria-hidden="true">🗂</div>
                  <div class="shared-meta">
                    <div class="shared-name" title={s.file.name}>{s.file.name}</div>
                    <div class="shared-sub">
                      {humanBytes(s.file.size)} · {s.me ? "you" : s.who}{#if !s.me && s.machine}&nbsp;· {s.machine}{/if}
                    </div>
                    {#if dl && dl.state === "fetching"}
                      <div class="dl-bar"><span style="width:{dl.total ? Math.min(100, Math.round((dl.done / dl.total) * 100)) : 0}%"></span></div>
                    {:else if dl && dl.state === "done"}
                      <div class="dl-note ok">Saved to Downloads</div>
                    {:else if dl && dl.state === "error"}
                      <div class="dl-note err">{dl.note}</div>
                    {/if}
                  </div>
                  {#if s.me}
                    <button
                      class="shared-act stop"
                      title="Stop sharing this file"
                      aria-label="Stop sharing"
                      onclick={() => app.unshareRoomFile(room.id, s.file.token)}>✕</button
                    >
                  {:else}
                    <button
                      class="shared-act"
                      disabled={dl?.state === "fetching"}
                      title="Download to your Downloads folder"
                      onclick={() => app.downloadSharedFile(s.from, s.file)}
                    >
                      {dl?.state === "fetching" ? "…" : dl?.state === "done" ? "Again" : "Download"}
                    </button>
                  {/if}
                </div>
              {:else}
                <p class="files-empty">
                  Nothing shared yet. <b>Share a file</b> and everyone in the call can download
                  it{#if !app.backendConnected}&nbsp;(demo mode — sharing needs the desktop app){/if}.
                </p>
              {/each}
            </div>
          </aside>
        {/if}
      </div>

      <footer class="bar">
        <div class="bar-group">
          <button
            class="ctl"
            class:on={app.roomMic}
            onclick={() => app.toggleRoomMic()}
            title="Talk to the room — your microphone to the members' speakers (m)"
          >
            <span class="ctl-icon" class:slashed={!app.roomMic}>🎙</span>
            <span class="ctl-label">{app.roomMic ? "Mute" : "Unmute"}</span>
          </button>
          <button
            class="ctl"
            class:on={app.roomCam}
            onclick={() => app.toggleRoomCam()}
            title="Send your camera to the room — members see it as a live tile (v)"
          >
            <span class="ctl-icon" class:slashed={!app.roomCam}>📷</span>
            <span class="ctl-label">Camera</span>
          </button>
        </div>
        <div class="bar-sep"></div>
        <div class="bar-group">
          <div class="files-wrap">
            <button
              class="ctl"
              class:on={app.roomScreen}
              onclick={shareScreen}
              title={app.roomScreenSources.length > 1
                ? "Share a screen with the room — pick which monitor"
                : "Share this machine's screen with the room"}
            >
              <span class="ctl-icon">🖥</span>
              <span class="ctl-label">{app.roomScreen ? "Stop share" : "Share screen"}</span>
            </button>
            {#if screenPickerOpen && !app.roomScreen}
              <div class="files-pick">
                <div class="pick-head">Share which screen?</div>
                {#each app.roomScreenSources as s (s.id)}
                  <button class="files-pick-row" onclick={() => pickScreen(s.id)}>
                    🖥 {s.label}{#if s.default}&nbsp;<span class="pick-tag">main</span>{/if}
                  </button>
                {/each}
              </div>
            {/if}
          </div>
          <button
            class="ctl"
            class:on={app.roomSound}
            onclick={() => app.toggleRoomSound()}
            title="Share what this machine is playing (its system audio) — NOT your microphone"
          >
            <span class="ctl-icon">🔊</span>
            <span class="ctl-label">Share sound</span>
          </button>
          <button
            class="ctl"
            class:on={app.roomControl}
            onclick={() => app.toggleRoomControl()}
            title="Let members click and type on this machine (owner/fleet members only — others' input is dropped)"
          >
            <span class="ctl-icon">🕹</span>
            <span class="ctl-label">Share control</span>
          </button>
          <button
            class="ctl"
            onclick={shareFiles}
            title="Add files to the room's Shared Files — members can download them while you're here (it's not a file browser)"
          >
            <span class="ctl-icon">🗂</span>
            <span class="ctl-label">Share files</span>
          </button>
        </div>
        <div class="bar-spacer"></div>
        <div class="bar-group">
          <button class="ctl" class:lit={app.roomChatOpen} onclick={toggleChat} title="Room chat">
            <span class="ctl-icon">💬</span>
            <span class="ctl-label">Chat</span>
            {#if unread > 0}<span class="ctl-badge">{unread}</span>{/if}
          </button>
          <button class="ctl" class:lit={app.roomFilesOpen} onclick={toggleFiles} title="Shared Files — what's been shared into this call">
            <span class="ctl-icon">🗂</span>
            <span class="ctl-label">Files</span>
            {#if sharedFiles.length > 0}<span class="ctl-badge">{sharedFiles.length}</span>{/if}
          </button>
          <button class="ctl" class:lit={app.roomPeopleOpen} onclick={togglePeople} title="Who's here, invites, and asks to join">
            <span class="ctl-icon">👥</span>
            <span class="ctl-label">People</span>
            {#if knocks.length > 0}<span class="ctl-badge warn">{knocks.length}</span>{/if}
          </button>
        </div>
        <div class="bar-sep"></div>
        <button class="leave" onclick={() => app.leaveRoom(room.id)} title="Hang up — your shares stop and members see you go">
          Leave
        </button>
      </footer>
    </section>
  </div>
{/if}

<style>
  .room-wrap {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    padding: 1.2rem;
    background: rgba(0, 0, 0, 0.45);
    backdrop-filter: blur(3px);
    z-index: 25;
    animation: fade 0.12s ease;
  }
  /* A dedicated room window: the call *is* the window. */
  .room-wrap.windowed {
    position: fixed;
    padding: 0;
    background: var(--bg);
    backdrop-filter: none;
  }
  @keyframes fade {
    from {
      opacity: 0;
    }
  }
  .room {
    width: min(76rem, 100%);
    height: min(48rem, 100%);
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
  }
  .windowed .room {
    width: 100%;
    height: 100%;
    border: none;
    border-radius: 0;
    box-shadow: none;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.55rem 0.8rem;
    border-bottom: 1px solid var(--line);
    flex-wrap: wrap;
  }
  .head-spacer {
    flex: 1;
  }
  .title {
    font-weight: 750;
    font-size: 1rem;
    white-space: nowrap;
  }
  .title-btn {
    border: none;
    background: transparent;
    color: var(--ink);
    padding: 0.1rem 0.3rem;
    border-radius: var(--r-sm);
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
  }
  .title-btn:hover {
    background: var(--surface-2);
  }
  .pencil {
    font-size: 0.78rem;
    color: var(--ink-faint);
  }
  .title-btn:hover .pencil {
    color: var(--ink);
  }
  .title-input {
    font-weight: 750;
    font-size: 1rem;
    font-family: inherit;
    color: var(--ink);
    background: var(--surface-2);
    border: 1px solid var(--accent);
    border-radius: var(--r-sm);
    padding: 0.15rem 0.45rem;
    min-width: 12rem;
  }
  .host-chip,
  .access-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.7rem;
    font-weight: 650;
    color: var(--ink-faint);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.16rem 0.55rem;
    white-space: nowrap;
  }
  .host-chip.mine {
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-color: oklch(0.64 0.255 350 / 0.35);
  }
  button.access-chip {
    cursor: pointer;
  }
  button.access-chip:hover {
    border-color: var(--line-strong);
    color: var(--ink-soft);
  }
  .access-chip.open {
    color: var(--ok);
    background: var(--ok-soft);
    border-color: oklch(0.8 0.17 150 / 0.3);
  }
  .timer {
    font-size: 0.74rem;
    font-weight: 650;
    font-variant-numeric: tabular-nums;
    color: var(--ink-soft);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.16rem 0.55rem;
  }
  .mini {
    font-weight: 800;
  }

  /* The "you are sharing" truth bar — green, persistent, with the red
     stop right there (the convention every call app converged on). */
  .share-banner {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.55rem;
    padding: 0.32rem 0.8rem;
    font-size: 0.78rem;
    font-weight: 650;
    color: var(--ok);
    background: var(--ok-soft);
    border-bottom: 1px solid oklch(0.8 0.17 150 / 0.3);
  }
  .share-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--ok);
    animation: pulse 1.6s ease-out infinite;
  }
  @keyframes pulse {
    0% {
      box-shadow: 0 0 0 0 oklch(0.8 0.17 150 / 0.5);
    }
    70% {
      box-shadow: 0 0 0 6px oklch(0.8 0.17 150 / 0);
    }
    100% {
      box-shadow: 0 0 0 0 oklch(0.8 0.17 150 / 0);
    }
  }
  .stop-share {
    border: none;
    background: var(--danger);
    color: #fff;
    font-weight: 700;
    font-size: 0.72rem;
    border-radius: var(--r-pill);
    padding: 0.18rem 0.65rem;
  }
  .stop-share:hover {
    filter: brightness(1.1);
  }

  .body {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .stage {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    padding: 0.8rem;
    gap: 0.6rem;
    overflow: hidden;
    background:
      radial-gradient(1200px 400px at 50% -10%, oklch(0.62 0.2 292 / 0.1), transparent),
      oklch(0.115 0.02 285);
  }

  /* Shares on the stage, people on the strip below — speaker layout. */
  .share-stage {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(22rem, 100%), 1fr));
    grid-auto-rows: minmax(0, 1fr);
    gap: 0.7rem;
  }
  .share-stage.single {
    grid-template-columns: minmax(0, 1fr);
  }
  .filmstrip {
    flex-shrink: 0;
    display: flex;
    gap: 0.5rem;
    overflow-x: auto;
    padding-bottom: 0.1rem;
  }

  /* Nobody sharing: the gallery — a tile per person in the call. */
  .gallery {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(13rem, 100%), 1fr));
    grid-auto-rows: minmax(0, 1fr);
    gap: 0.7rem;
    align-items: stretch;
    overflow-y: auto;
  }
  /* One or two people: video-tile proportions, centred — not a column
     stretched to the stage. */
  .gallery.few {
    grid-template-columns: repeat(auto-fit, minmax(min(13rem, 100%), 36rem));
    grid-auto-rows: minmax(0, 23rem);
    justify-content: center;
    align-content: center;
  }
  .person {
    position: relative;
    display: grid;
    place-items: center;
    background: oklch(0.165 0.024 285);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    min-height: 8rem;
    overflow: hidden;
    transition: border-color 0.15s ease, box-shadow 0.15s ease;
  }
  .gallery.few .person {
    min-height: 11rem;
  }
  .person.speaking {
    border-color: var(--ok);
    box-shadow: 0 0 0 1.5px var(--ok), 0 0 18px oklch(0.8 0.17 150 / 0.25);
  }
  .person.offline {
    opacity: 0.55;
  }
  .person.compact {
    min-height: 0;
    height: 4.8rem;
    width: 8.5rem;
    flex-shrink: 0;
  }
  .avatar {
    width: 3.2rem;
    height: 3.2rem;
    border-radius: 50%;
    display: grid;
    place-items: center;
    font-weight: 800;
    font-size: 1.15rem;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border: 1px solid oklch(0.64 0.255 350 / 0.3);
    user-select: none;
  }
  .person.compact .avatar {
    width: 1.9rem;
    height: 1.9rem;
    font-size: 0.72rem;
    margin-bottom: 1rem;
  }
  .sending {
    position: absolute;
    top: 0.45rem;
    left: 0.45rem;
    right: 0.45rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
  }
  .person.compact .sending {
    display: none;
  }
  .send-chip {
    font-size: 0.64rem;
    font-weight: 650;
    color: var(--ink-soft);
    background: rgba(0, 0, 0, 0.5);
    border-radius: var(--r-pill);
    padding: 0.12rem 0.45rem;
  }
  .send-chip.ok {
    color: var(--ok);
  }
  .nameplate {
    position: absolute;
    left: 0.45rem;
    bottom: 0.45rem;
    max-width: calc(100% - 0.9rem);
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
    border-radius: var(--r-pill);
    padding: 0.16rem 0.55rem;
    font-size: 0.72rem;
    white-space: nowrap;
    overflow: hidden;
  }
  .person.compact .nameplate {
    left: 0.3rem;
    bottom: 0.3rem;
    padding: 0.1rem 0.4rem;
    font-size: 0.62rem;
  }
  .plate-who {
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .plate-machine,
  .line-machine,
  .r-machine {
    color: var(--ink-faint);
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* The muted state nobody should ever have to guess: a red-slashed mic
     right on the nameplate. */
  .mic-state {
    position: relative;
    flex-shrink: 0;
    line-height: 1;
  }
  .mic-state.muted {
    opacity: 0.85;
  }
  .mic-state.muted::after {
    content: "";
    position: absolute;
    left: 50%;
    top: 50%;
    width: 130%;
    height: 2px;
    background: var(--danger);
    border-radius: 2px;
    transform: translate(-50%, -50%) rotate(-45deg);
    box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.45);
  }
  .mic-state.live {
    filter: drop-shadow(0 0 4px oklch(0.8 0.17 150 / 0.8));
  }
  /* ---- right-side panels (chat · people) ---- */
  .side {
    width: 17.5rem;
    flex-shrink: 0;
    border-left: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    background: var(--surface-2);
    min-height: 0;
  }
  .side-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.5rem 0.6rem;
    border-bottom: 1px solid var(--line);
  }
  .side-head h4 {
    margin: 0;
    font-size: 0.78rem;
    color: var(--ink-soft);
  }
  .side-x {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    font-size: 0.72rem;
    padding: 0.1rem 0.3rem;
    border-radius: var(--r-sm);
  }
  .side-x:hover {
    color: var(--ink);
    background: var(--surface);
  }
  .chat-log {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.6rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .line {
    font-size: 0.8rem;
    line-height: 1.35;
  }
  .line-who {
    font-weight: 700;
  }
  .line.mine .line-who {
    color: var(--accent-ink);
  }
  .line-machine {
    font-size: 0.7rem;
  }
  .line-at {
    margin-left: 0.3rem;
    font-size: 0.66rem;
    color: var(--ink-faint);
  }
  .line-text {
    color: var(--ink-soft);
    overflow-wrap: anywhere;
  }
  .chat-empty {
    font-size: 0.76rem;
    color: var(--ink-faint);
    text-align: center;
    margin-top: 1rem;
  }
  .chat-send {
    display: flex;
    gap: 0.4rem;
    padding: 0.5rem;
    border-top: 1px solid var(--line);
  }
  .chat-send input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    padding: 0.35rem 0.7rem;
    font-size: 0.8rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .people-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.6rem;
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
  }
  .knocks {
    border: 1px solid oklch(0.79 0.14 75 / 0.4);
    background: var(--warn-soft);
    border-radius: var(--r-md);
    padding: 0.5rem;
  }
  .knocks h5,
  .invite-block h5 {
    margin: 0 0 0.4rem;
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
  }
  .knock-row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
  }
  .knock-who {
    flex: 1;
    min-width: 0;
    font-size: 0.78rem;
    font-weight: 650;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .roster {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .roster li {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.78rem;
    padding: 0.25rem 0.3rem;
    border-radius: var(--r-sm);
  }
  .roster li:hover {
    background: var(--surface);
  }
  .roster li.away {
    color: var(--ink-soft);
  }
  .r-who {
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .r-state {
    margin-left: auto;
    font-size: 0.64rem;
    color: var(--ink-faint);
    white-space: nowrap;
  }
  .m-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .m-dot.on {
    background: var(--ok);
  }
  .m-remove {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    font-size: 0.66rem;
    padding: 0 0.1rem;
    line-height: 1;
  }
  .roster li .m-remove {
    margin-left: auto;
  }
  .roster li.away .m-remove {
    margin-left: 0.2rem;
  }
  .m-remove:hover {
    color: var(--danger);
  }
  .invite-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    width: 100%;
    border: none;
    background: transparent;
    color: var(--ink);
    text-align: left;
    font-size: 0.78rem;
    padding: 0.3rem 0.3rem;
    border-radius: var(--r-sm);
  }
  .invite-row:hover {
    background: var(--surface);
  }
  .fine {
    margin: 0;
    font-size: 0.68rem;
    color: var(--ink-faint);
    line-height: 1.45;
  }
  /* ---- the bottom control bar ---- */
  .bar {
    border-top: 1px solid var(--line);
    padding: 0.5rem 0.8rem;
    display: flex;
    align-items: stretch;
    gap: 0.55rem;
    row-gap: 0.3rem;
    flex-wrap: wrap;
    background: oklch(0.15 0.023 285);
  }
  .bar-group {
    display: flex;
    gap: 0.35rem;
  }
  .bar-sep {
    width: 1px;
    background: var(--line);
    margin: 0.25rem 0;
  }
  .bar-spacer {
    flex: 1;
  }
  .ctl {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.18rem;
    min-width: 4.6rem;
    border: 1px solid transparent;
    background: transparent;
    color: var(--ink);
    border-radius: var(--r-md);
    padding: 0.35rem 0.5rem 0.3rem;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .ctl:hover:not(:disabled) {
    background: var(--surface-2);
    border-color: var(--line);
  }
  .ctl:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .ctl.on {
    border-color: oklch(0.8 0.17 150 / 0.45);
    background: var(--ok-soft);
  }
  .ctl.lit {
    border-color: var(--line-strong);
    background: var(--surface-2);
  }
  .ctl-icon {
    position: relative;
    font-size: 1.1rem;
    line-height: 1;
  }
  /* Off mic / off camera read at a glance: the red slash. */
  .ctl-icon.slashed::after {
    content: "";
    position: absolute;
    left: 50%;
    top: 50%;
    width: 135%;
    height: 2.5px;
    background: var(--danger);
    border-radius: 2px;
    transform: translate(-50%, -50%) rotate(-45deg);
    box-shadow: 0 0 0 1.5px oklch(0.15 0.023 285);
  }
  .ctl-label {
    font-size: 0.68rem;
    font-weight: 650;
    color: var(--ink-soft);
    white-space: nowrap;
  }
  .ctl.on .ctl-label {
    color: var(--ok);
  }
  .ctl-badge {
    position: absolute;
    top: 0.15rem;
    right: 0.3rem;
    background: var(--accent);
    color: #fff;
    font-size: 0.6rem;
    font-weight: 700;
    border-radius: var(--r-pill);
    padding: 0.02rem 0.32rem;
    line-height: 1.3;
  }
  .ctl-badge.warn {
    background: var(--warn);
    color: var(--bg);
  }
  .leave {
    align-self: center;
    border: none;
    background: var(--danger);
    color: #fff;
    font-weight: 750;
    font-size: 0.82rem;
    border-radius: var(--r-md);
    padding: 0.55rem 1.1rem;
  }
  .leave:hover {
    filter: brightness(1.12);
  }
  .files-wrap {
    position: relative;
  }
  .files-pick {
    position: absolute;
    bottom: calc(100% + 0.35rem);
    left: 0;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-md);
    padding: 0.3rem;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 11rem;
    z-index: 5;
  }
  .files-pick-row {
    border: none;
    background: transparent;
    color: var(--ink);
    text-align: left;
    font-size: 0.78rem;
    padding: 0.35rem 0.5rem;
    border-radius: var(--r-sm);
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 0.3rem;
  }
  .files-pick-row:hover {
    background: var(--surface-2);
  }
  .pick-head {
    font-size: 0.66rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
    padding: 0.2rem 0.5rem 0.3rem;
  }
  .pick-tag {
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.02rem 0.34rem;
  }

  /* ---- the Shared Files sidebar ---- */
  .files-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 0.6rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .share-add {
    border: 1px dashed var(--line-strong);
    background: var(--surface);
    color: var(--ink-soft);
    font-weight: 650;
    font-size: 0.78rem;
    border-radius: var(--r-md);
    padding: 0.5rem;
    flex-shrink: 0;
  }
  .share-add:hover {
    border-color: var(--accent);
    color: var(--ink);
  }
  .shared-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.45rem;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
  }
  .shared-row.mine {
    border-color: oklch(0.64 0.255 350 / 0.3);
    background: var(--accent-soft);
  }
  .shared-icon {
    font-size: 1.1rem;
    flex-shrink: 0;
  }
  .shared-meta {
    flex: 1;
    min-width: 0;
  }
  .shared-name {
    font-size: 0.8rem;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .shared-sub {
    font-size: 0.68rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dl-bar {
    margin-top: 0.3rem;
    height: 4px;
    border-radius: 2px;
    background: var(--surface-2);
    overflow: hidden;
  }
  .dl-bar span {
    display: block;
    height: 100%;
    background: var(--accent);
    transition: width 0.2s ease;
  }
  .dl-note {
    margin-top: 0.2rem;
    font-size: 0.66rem;
  }
  .dl-note.ok {
    color: var(--ok);
  }
  .dl-note.err {
    color: var(--danger);
    overflow-wrap: anywhere;
  }
  .shared-act {
    flex-shrink: 0;
    border: 1px solid var(--line-strong);
    background: var(--surface-2);
    color: var(--ink);
    font-weight: 650;
    font-size: 0.72rem;
    border-radius: var(--r-pill);
    padding: 0.25rem 0.6rem;
  }
  .shared-act:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .shared-act:disabled {
    opacity: 0.5;
  }
  .shared-act.stop {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    padding: 0.1rem 0.3rem;
  }
  .shared-act.stop:hover {
    color: var(--danger);
  }
  .files-empty {
    font-size: 0.76rem;
    color: var(--ink-faint);
    text-align: center;
    margin-top: 0.5rem;
    line-height: 1.5;
  }
  .files-empty b {
    color: var(--ink-soft);
  }
</style>
