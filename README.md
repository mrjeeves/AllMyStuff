<div align="center">

<img src="docs/design/logo.png" width="104" alt="AllMyStuff logo" />

# AllMyStuff

**Everything you own, wired together.**

Your computers, KVMs, files, terminals, and local sites in one place.

[![Release](https://img.shields.io/github/v/release/mrjeeves/AllMyStuff?label=release&color=success)](https://github.com/mrjeeves/AllMyStuff/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-informational.svg)](#install)
[![Built on MyOwnMesh](https://img.shields.io/badge/mesh-MyOwnMesh-6c5ce7.svg)](https://github.com/mrjeeves/MyOwnMesh)

<img src="docs/design/allmystuff-app.svg" width="720" alt="AllMyStuff showing a fleet of computers and KVMs, with controls for remote access, files, drives, and power" />

</div>

AllMyStuff is a desktop app for reaching the machines that are yours and the
things other people have shared with you. Remote control a screen, open a real
terminal, move files, map a folder into the native filesystem, reach a local
web service, or work through a KVM when the operating system is not available.

The graph is the home screen. Your fleet stays together, while devices visible
through a mesh or share remain separate and expose only the actions that
relationship allows.

There is no cloud copy of your stuff and no VPN to configure. Connections use
an end-to-end encrypted [MyOwnMesh](https://github.com/mrjeeves/MyOwnMesh)
route. Peers connect directly when they can and use an authenticated relay when
NAT or a firewall leaves no direct path.

AllMyStuff is free, open source, and runs on macOS, Linux, and Windows.

## Install

**macOS and Linux**

```sh
curl -fsSL https://allmystuff.works/install.sh | sh
```

**Windows**

```powershell
irm https://allmystuff.works/install.ps1 | iex
```

The installer verifies the release, installs the desktop app and command-line
tools, and brings the pinned MyOwnMesh runtime with it. Prebuilt `.dmg`, `.msi`,
`.deb`, and `.AppImage` bundles, plus portable archives, are available on the
[releases page](https://github.com/mrjeeves/AllMyStuff/releases).

## Your first fleet

Install AllMyStuff on each computer you want to own as a group. On the local
network, choose **Make claimable** under that computer's **This Device** card,
then select it from another AllMyStuff machine and choose **Claim this device**.
Claiming adds it to your fleet and gives it the fleet relationship used for
remote control, terminals, files, drives, sites, updates, and device management.

Once the machines are in your fleet, they do not need to remain on the same
LAN. Open AllMyStuff and choose **Remote**, **Files**, **Drives**, or
**Settings** on a device card. Normal mode keeps the graph and its common
actions up front. Advanced mode exposes the fuller routing and device surfaces.

For task-by-task walkthroughs, see [Using AllMyStuff](docs/USING-ALLMYSTUFF.md).
The [documentation map](docs/README.md) separates user help from contributor,
architecture, and maintainer material.

## What it does

- **Remote control.** View and control every shared display, switch between
  screens, carry text, images, and files through the synchronized clipboard,
  and drop local files onto the remote desktop. Relative mouse capture supports
  games and other applications that consume raw movement. Pop any monitor into
  its own native window, then arrange several remote monitors across your local
  displays so the whole remote desk stays visible at once.
- **Terminals.** Open a real shell without configuring SSH, keys, or forwarded
  ports. Terminal sessions can be attached from more than one machine when you
  want to follow the same shell together.
- **Fleetfiles, Files, and native drives.** Browse the fleet-wide logical
  Fleetfiles tree without choosing which device holds each body. Large folders
  are paged and virtualized; opens hydrate verified content on demand; retained
  versions can be restored from an online replica. Use Local copies only to
  inspect a physical device path. You can also browse explicit remote-machine
  grants or map an authorized folder/drive into the operating system: a drive
  letter in Windows, a mounted volume on macOS, or a mount point on Linux. A
  mapping is one-way, but both affected machines show the same relationship.
- **Sites.** Publish only the local web services you mean to expose, then open
  them from another authorized machine even when the service itself listens on
  loopback.
- **KVMs.** Use NanoKVM and NanoKVM-Pro devices for video, keyboard, mouse,
  power control, Wi-Fi, updates, and BIOS access. Authorized ISO, IMG, and
  removable USB media can be presented to the attached computer for operating
  system installation or firmware work.
- **Sharing.** Share a capability with a person without adding their computer
  to your fleet. Screen, control, files, cameras, microphones, and sites keep
  their own permissions. A share from you does not silently create a share back
  to you.
- **Rooms.** Bring several people and machines into a temporary collaboration
  surface with chat, screens, control, cameras, microphones, and shared files.
- **Always On.** Run the node as a system service so a machine stays reachable
  before login and after reboot. The app also checks and repairs its pinned
  runtime components during updates.

The detailed native-drive and KVM media behavior is documented in
[Native drive mapping over the mesh](docs/DRIVE-MAPPING.md).

## Fleets, meshes, shares, and rooms

These ideas are intentionally separate:

| Concept | What it means |
|---|---|
| **Fleet** | The devices you own together. Membership is signed and carries an owner, manager, or member role. |
| **Mesh** | A private reachability space. Being visible on a mesh does not grant access to a device. |
| **Share** | A one-way permission from one person to another for specific capabilities. |
| **Room** | A shared session where invited members can use only the capabilities offered to that room. |

That separation matters. Seeing a computer is not permission to control it,
sharing your files does not expose theirs, and joining a room does not make its
members part of your fleet.

## AMSTerm

`amst` comes with AllMyStuff and opens a mesh terminal from the command line:

```sh
amst                         # a shell on this machine
amst Tracy-Laptop            # a new shell on another fleet machine
amst Tracy-Laptop --sessions # list its open terminal sessions
amst Tracy-Laptop --attach term-3
```

On Windows, the installer also adds an AMSTerm launcher, shortcuts, and an
**AMSTerm here** folder menu. If this machine's node is not running, `amst`
starts the desktop app or the installed headless node as appropriate.

## Headless machines

Run a server, build machine, or other computer without the desktop interface:

```sh
allmystuff serve
```

Keep it available across logins and reboots:

```sh
allmystuff service install
```

The desktop app and `allmystuff serve` run the same node engine and speak the
same protocol.

## How it works

AllMyStuff is written in Rust with a [Tauri](https://tauri.app) and
[Svelte](https://svelte.dev) desktop interface. Its node engine owns device
inventory, authorization, sessions, media, input, terminals, files, drive
mapping, sites, KVM integration, and the local control API. MyOwnMesh provides
identity, discovery, encrypted transport, and relay fallback.

The graph model and protocol live in shared Rust crates. The desktop app and
headless node use those same crates, while the UI mirrors the state it needs to
stay responsive.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the crate map, data flow, and
persistent-state layout.

## Project status

AllMyStuff is under active development and ships frequent releases. The
desktop app and headless node are built and tested on macOS, Linux, and Windows.
The mobile client has a runnable Tauri shell and shared core, but device
validation and store releases are still in progress. See
[docs/MOBILE.md](docs/MOBILE.md) for its current state.

Please use [GitHub Issues](https://github.com/mrjeeves/AllMyStuff/issues) for
reproducible bugs and feature requests.

## Build from source

Install [`just`](https://just.systems), then:

```sh
just setup       # install development prerequisites
just dev         # run the complete desktop app with hot reload
just check       # run the library, node, and front-end checks
just gui-build   # build the native desktop bundle
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for platform requirements, CLI details,
and the repository workflow.

## License

[MIT](LICENSE)
