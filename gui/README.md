# AllMyStuff GUI

Tauri 2 + Svelte 5. The friendly graph that scans your machine and wires
your stuff together. On desktop it is a **client** of the per-machine node
(`allmystuff-serve`) over its control socket. It never runs the engine
itself. The one exception is the phone, where no separate process is
allowed: `mobile/` embeds the same engine in-process (see
[docs/MOBILE.md](../docs/MOBILE.md)).

## Layout

```
gui/
├── src/                      # Svelte 5 front-end: ONE bundle, shared by
│   │                         #   desktop, mobile, and the web preview
│   ├── ui/
│   │   ├── App.svelte        # shell: top bar + stage + sheets; ?console=/?terminal=/
│   │   │                     #   ?files=/?room=/?video=/?cec=/?chat= render a popout host
│   │   ├── Graph.svelte      # the node graph (radial/grid/list; pan/pinch/tap on touch)
│   │   ├── NodeDrawer.svelte # click a node → stats, devices, connections, sharing
│   │   ├── Console.svelte    # remote screen (+ console-touch.ts trackpad model,
│   │   │                     #   ConsoleKeys.svelte soft keyboard on mobile)
│   │   ├── Terminal.svelte   # xterm.js shells (+ TerminalKeys.svelte strip on mobile)
│   │   ├── Files.svelte      # remote file browser
│   │   ├── RoomsTab/RoomPanel/RoomTile/RoomHost   # virtual rooms
│   │   ├── HelpTab.svelte / CecChatWindow.svelte  # CEC Support (technician side)
│   │   ├── SitesTab / LayersSheet / ClaimSheet / ShareSheet / ShareFlow
│   │   ├── SettingsPanel.svelte + settings/       # device, fleet, sharing, meshes, support
│   │   └── Toasts.svelte
│   ├── types.ts              # TS mirror of the graph model + visual helpers
│   ├── catalog.ts            # routing + authorization rules (port of allmystuff-graph)
│   ├── store.svelte.ts       # app state (runes) + the connect/share/room/fleet/cec verbs
│   ├── console-touch.ts      # the console's touch pointer model (trackpad, not tap-the-pixel)
│   ├── swipe.ts              # swipe-to-close for the docked panels
│   ├── mock.ts               # demo graph so the app is alive with no backend
│   └── tauri.ts              # backend bridge (degrades to web mode; isMobile() posture)
├── src-tauri/                # DESKTOP Tauri shell in its own Cargo workspace.
│                             #   A thin client of the node socket: command
│                             #   passthroughs + the event pump + windows/tray/
│                             #   updater/service glue
└── mobile/                   # MOBILE (iOS/Android) Tauri shell in its own
                              #   workspace. Embeds the myownmesh daemon + the
                              #   capture-less node engine IN-PROCESS and answers
                              #   the same command surface (docs/MOBILE.md)
```

The mesh node itself, including the control-socket transport, daemon spawner,
and every media plane, lives in the **`allmystuff-node`** crate (`../node`),
which `allmystuff serve` runs headless. Both shells are webview wiring on
top of the same engine: the desktop reaches it over a socket, the phone
links it.

## Develop

From the repository root, use the same entry points as CI:

```sh
just dev          # full desktop app with hot reload
just gui-check    # front-end tests, types, and production build
just gui-build    # installable desktop bundle
```

When working only on the Svelte front end, `pnpm test`, `pnpm check`, and
`pnpm build` are also available from this directory.

The front-end is the part that builds anywhere. The Tauri backend links the
system webview, so building the desktop app on Linux needs `libgtk-3-dev` and
`libwebkit2gtk-4.1-dev`; macOS and Windows use their built-in webviews. The
mobile shell's builds are documented in [docs/MOBILE.md](../docs/MOBILE.md).

## Web mode vs desktop vs mobile

`tauri.ts` detects whether a Tauri backend is present. With one, `scan_self`
replaces the demo "this device" with a real scan and the mesh commands light
up. Without one (a plain `pnpm dev` in a browser, or this repo's CI build),
the demo graph stands in so the whole experience, including clicking nodes,
drawing connections, the share sheet, and rooms, works offline.

Mobile is a *runtime posture* of the same bundle, not a fork: `isMobile()`
keeps every surface in-app (desktop pops consoles/terminals/files/rooms out
into OS windows), the touch modules take over the pointer work, and
`html.is-mobile` + media queries compact the chrome. Desktop-only affordances
(the updater tab, the Always-On service, network export's save dialog) hide
themselves on the phone.

## Backend events

The backend pumps the engine's event stream out as `allmystuff://…` Tauri
events (and `cec://…` for CEC Support) and tracks connection state under
`allmystuff://subscription` (query `mesh_subscription_state` after
registering the listener to avoid the fire-and-forget race, following the MyOwnMesh
GUI pattern). On desktop that pump crosses the node socket; on mobile the
engine's `UiSink` emits straight onto the webview bus. Same events, same
names, either way.
