# Native drive mapping over the mesh

This is the implementation and security specification. For ordinary mapping
steps, start with [Files and drives](USING-ALLMYSTUFF.md#files-and-drives).

## Decision

AllMyStuff maps an explicitly selected folder as a real operating-system
drive. Windows receives an ordinary drive letter, macOS receives a mounted
volume, and Linux receives a filesystem mount point. Native applications use
the result directly; it is not an AllMyStuff-only file browser.

Windows uses its built-in SMB client. The source starts a private SMB3 server
confined to the granted folder, and AMS tunnels its TCP stream across the live
Storage route. The receiver maps a private loopback endpoint with Windows'
SMB alternate-port support. Windows WebClient's file-size ceiling does not
apply. Fleetfiles keeps its existing Cloud Files provider. No additional
filesystem driver, Windows account, OS file share, firewall change, WinFsp,
or rclone is required for Windows mapped folders.

On macOS and Linux the receiving node runs a loopback-only WebDAV server that
translates filesystem operations into the same mesh file protocol. macOS
mounts it with its built-in `mount_webdav`, and Linux with
an owned foreground `rclone mount` process over FUSE. The WebDAV server is never exposed to the LAN and
the file bytes still travel only on the live AllMyStuff route (direct, STUN, or
TURN).

## Distribution compatibility

| Receiving platform | Native mapping | Installation requirements |
| --- | --- | --- |
| Windows x64 | SMB3 over AMS | Windows 11 24H2 / Server 2025 or later; installed AMS service or elevated node; signed-in user |
| macOS ARM64 / x64 | Existing WebDAV volume | Built-in `mount_webdav`; interactive user |
| Linux ARM64 / x64 | Existing WebDAV through FUSE | `rclone`, `fuse3`, accessible `/dev/fuse`; interactive user or user service |
| iOS / Android | No native drive mount | Mobile remains a file browser/controller; no desktop drive letter or mount is promised |

Windows, macOS, and Linux can all supply a folder to a Windows SMB receiver.
Both endpoints need an AMS build advertising `smb-drives-v1`. This change does
not switch the macOS/Linux receiving adapters to SMB. Fleetfiles stays separate;
its Windows provider remains Cloud Files.

The Windows app can run on versions older than the SMB receiver requirement.
On those versions, mapping returns an explicit compatibility error. A read-only
OS/token preflight runs before removing any legacy Cloud Files drive alias.
There is no silent WebClient fallback or driver installation.

Linux releases retain the Ubuntu 22.04 / glibc 2.35 build floor. The `.deb` and
`.rpm` packages declare `rclone` and `fuse3` dependencies. AppImage and portable
archives do not bundle those mount helpers: install them through the host's
package manager. `scripts/install.sh` attempts that automatically on apt-based
systems and gives instructions elsewhere. Missing helpers or restricted FUSE
access produce a mount error with recovery instructions; a packaged GUI alone
does not prove a usable mount.

PR CI builds/tests the shared node and desktop backend on every native release
runner: Ubuntu 22.04 x64/ARM64, macOS 15 ARM64/x64, and Windows x64. Ubuntu 24.04
also checks the optional FFmpeg encoder, and latest Ubuntu checks the GUI. CI and
release use the same Linux dependency action, including Jammy's PipeWire setup.
The node, desktop, and mobile locks require rustix 1.1.6 or later for the SMB
backend's Linux extended-attribute APIs. Locked CI checks prevent a newer local
resolution from hiding an incompatible committed lockfile.

Those checks validate compilation, unit behavior, and private SMB negotiation.
They do not exercise Finder/Explorer, OS mount permissions, every Linux distro,
or a real two-machine transfer. The manual [platform and transfer matrix](SMB-MAPPED-DRIVES-TESTING.md)
is required before claiming runtime interoperability or large-file support for
an untested source/receiver pair. The Android CI job checks the shell without
the embedded engine; it is not a full mobile-engine compatibility test.

## User contract

- From another machine's card or remote-control console, **Drives → Map new
  Drive** opens the local OS folder picker. The user may choose a whole drive
  or one folder.
- From **This Device → Drives**, the user first chooses an online fleet,
  shared, or support machine with Files access, browses that machine's real
  file session, and chooses the remote folder to mount here. A local native
  picker cannot browse a remote filesystem, so this distinction is explicit.
- A KVM's Drives button is its virtual-media surface. The user chooses an ISO,
  IMG, or whole removable USB disk from an eligible source machine; the KVM
  presents it to its attached computer as BIOS/UEFI-visible USB storage.
- The attached computer is the destination and is therefore excluded from the
  source list. This prevents a circular source that disappears when that
  computer reboots into an installer or firmware utility.
- The drive-letter field defaults to **Auto, next available**. A user may
  enter a particular `X:` instead. Enter, click-away, or Map Drive completes
  the form.
- A mapping is still one-way, but both affected machines show the same source,
  destination, label, mount point, and availability in every Drives surface.
  Creating the reverse direction is a second explicit mapping, not a default.
- Remove Mapping records one durable removal on both machines, then tears down
  the native drive and its mesh route. If one machine is asleep, the online
  machine retries that removal when its peer returns.

## Architecture

```text
source machine                                      receiving machine
┌─────────────────────────────┐                    ┌──────────────────────────┐
│ selected local folder       │                    │ Explorer / Finder / apps │
│ route-id → canonical root   │                    │ X:\ or native mount      │
│             │               │                    │ Windows SMB client       │
│ scoped SMB / FileEvent host │◄══ mesh route ═══►│ or Unix loopback WebDAV │
└─────────────────────────────┘  direct/STUN/TURN  │ scoped native adapter    │
                                                   └──────────────────────────┘
```

One mapping has a stable mapping ID and a `Storage` route:

```text
<source>:drive-map:<mapping-id> → <receiver>:storage-in
```

Both endpoints persist the mapping ID and its display relationship. The
receiver additionally persists the reconnect information required to restore
its OS mount. After the mount succeeds, it reports the actual selected drive
letter or mount point to the source, so both interfaces display the same name
and destination. Before offering, the source canonicalizes the chosen path and
binds it locally to the mapping's route. Multiple folders between the same pair
therefore remain independent.

The native adapter adds metadata and random-access read/write operations to
`FileEvent`. Replies for OS filesystem calls use a dedicated per-request
waiter instead of the GUI Files queue. Existing whole-machine Files sessions,
room shared-file downloads, and their permissions remain separate.

For Windows mappings the scoped files RPC negotiates a per-route SMB endpoint.
The source uses the pinned `smb-server` library's capability-confined local
filesystem backend. Every endpoint binds to loopback, serves one granted root,
and requires a fresh random password, SMB signing, and SMB3 encryption. No
credentials enter session snapshots or persistent lease files; debug formatting
redacts them. SMB leases are disabled because source applications can modify
the same files outside SMB.

The receiver's private loopback identity separates simultaneous SMB sessions.
Only the source's recorded endpoint port can be opened on that exact active
Storage route by that route's authenticated peer. General Sites grants and
arbitrary source ports are not exposed. Windows maps the endpoint through
`New-SmbGlobalMapping`, nonpersistently, restricted to the signed-in user's
account and LocalSystem. Credentials pass to PowerShell through stdin, never
through command arguments or disk. The source filesystem reports capacity and
handles file I/O directly; there is no separate mapped-folder sync cache.

Route teardown closes the SMB endpoint and all its tunnel connections. Existing
mapping relationships supply reconnect intent. Migration checks the previous
Cloud Files mapped-drive journal: unsent edits prevent that mapping's migration.
Old cache directories and journals are retained for recovery, never deleted.

## Lifecycle

1. Source selects and canonicalizes a folder, binds it to a fresh route id,
   and offers the Storage route with `DriveRouteOffer` metadata.
2. The receiver accepts under the normal Files authorization gate.
3. On activation, Windows negotiates the private source SMB endpoint, starts
   its mesh tunnel, and creates a native SMB network mapping. It selects the
   next unassigned letter from Z through A when Auto, with no generic letter
   reservations. Disconnected and remembered mappings are assigned, even if
   opening their root fails. Selection never disconnects a drive to free a letter.
   Exact owned endpoint leases
   gate cleanup; unrelated SMB or other drive assignments are preserved.
   macOS and Linux bind an ephemeral listener to `127.0.0.1:0`, build
   `RemoteDavFs`, and choose a native destination. macOS uses
   `~/AllMyStuff Drives/<name>`. A Linux system service uses
   `/mnt/allmystuff/<name>`; a user process uses
   `~/AllMyStuff Drives/<name>`. An explicit absolute mount point overrides
   those defaults.
4. Windows SMB bytes travel in bounded SiteFrames over the active Storage route.
   macOS/Linux WebDAV operations use scoped FileEvents over that route.
5. Ordinary route loss aborts the listener, cancels in-flight RPCs, and
   unmounts the native destination, but retains the relationship and reconnect
   intent. Both interfaces show it as unavailable until the peer returns.
6. Explicit removal deletes the relationship and reconnect intent on both
   endpoints. A crash-safe pending removal is retried until the peer confirms
   it, preventing a sleeping Windows machine from recreating an orphan drive.
7. If native mounting fails, the receiver tears the route down instead of
   showing a live connection line for a drive the operating system cannot use.
8. AllMyStuff records only the native mounts it created. After a crash or hard
   stop, startup cleanup unmounts those exact owned entries before restoring
   the durable mapping relationships. It never sweeps unrelated mounts.

## KVM install and firmware media

KVM media is deliberately not the desktop WebDAV mapping described above.
BIOS/UEFI cannot see a drive mounted by an operating-system agent. Instead:

1. The source opens an ISO/IMG, or reads a selected removable USB disk as a raw
   physical disk so its partition table and boot sectors are preserved.
2. The source streams those bytes directly through the KVM's authenticated
   mesh site tunnel. Large media never bounces through the controller webview.
3. NanoKVM or NanoKVM-Pro stages the image under `/data`, configures its Linux
   USB mass-storage gadget read-only, and presents it to the attached computer.
4. The KVM advertises the active source, label, and staged file in presence.
   AllMyStuff draws source to KVM as a live media connection while retaining
   the separate KVM to attached-computer physical tether.
5. Eject clears the gadget backing and presence metadata.

The source may be a fleet machine, an explicit Files share, or a currently
authorized support technician. It may never be the KVM's attached destination
computer. That invariant is enforced in both the picker and the node backend.

## Authorization and security

- A locally initiated outbound map is explicit user intent. An inbound map
  request is accepted only when the requester passes the source's Files gate:
  fleet/owner, an explicit Files share, or active CECSupport consent.
- The source canonicalizes the root itself; a receiver never binds its own
  claim about a source path.
- All viewer paths are virtual paths below the bound root. Parent traversal,
  prefixes, and symlink escapes are rejected. The virtual root cannot be
  renamed or deleted.
- WebDAV binds only to loopback. There is no new remotely reachable HTTP
  service and no separate credential to leak.
- Native operating-system drive mapping never depends on a KVM and never
  creates a Files share *to* a KVM. KVM virtual media is a separate,
  purpose-built path that terminates at the KVM's USB gadget.

## Compatibility and tradeoffs

- Windows receivers require Windows 11 24H2 / Server 2025 or later, an installed
  AMS service or elevated node, and an interactive user. Alternate SMB ports
  are required; older Windows versions receive an explicit compatibility error.
  The source must advertise `smb-drives-v1`; older sources need an AMS update.
  Windows, macOS, and Linux source folders use the same private SMB backend.
- Mapped-drive files follow network-filesystem behavior: the source must be
  reachable for I/O. Interrupted operations fail rather than becoming offline
  sync edits. Filesystem limits, source permissions, and free space still apply.
- Upgrade recovery recognizes legacy AMS WebDAV leases by their exact recorded
  endpoint. It never disconnects an unrelated SMB mapping to obtain a letter.
- macOS uses the operating system's built-in `mount_webdav` command and gives
  the volume the label selected in AllMyStuff.
- Linux uses `rclone mount` and FUSE as the signed-in user. Debian and RPM
  packages depend on `rclone` and `fuse3`; the portable installer attempts to
  install them through `apt-get`. No per-URL `/etc/fstab` entry or root mount
  privilege is needed. A restricted system service is still restricted:
  desktop mounts belong in the interactive user node or a user service.
- The node owns the Linux mount process and bounds startup and teardown.
  Cleanup verifies the exact mount identity before unmounting, including after
  a crash, rather than treating a reused mount path or a port prefix as ours.
  Linux unmounts use `fusermount3` or `fusermount`.
- Linux's write cache permits applications to seek while writing. Its private
  configuration and cache are keyed to the logical mapping so a changed route
  or loopback port does not change the cache identity. Closed writes are
  uploaded promptly; interrupted, unsent cache data is retained rather than
  removed during cleanup. The receiver must have space for buffered writes.
  See the [rclone mount documentation](https://rclone.org/commands/rclone_mount/)
  for the underlying FUSE and write-cache behavior.
- The macOS/Linux file RPC adapter does not preserve all source filesystem features.
  Hard links, alternate streams, and POSIX ownership are outside this mapping
  contract. macOS/Linux additionally have their WebDAV representation limits.
- Remote-source selection uses the already-authorized mesh Files session.
  Pretending a native dialog on this PC could browse another PC was rejected;
  so was temporarily exposing an entire remote disk merely to feed a picker.
- The receiver chooses Auto at activation because only it knows which drive
  letters are actually free. It reports that result to the source for display;
  the receiver's OS remains authoritative.

## Verification

- Unit-test scoped paths, symlink/parent escape rejection, metadata, ranged
  reads, and ranged writes.
- Test offer metadata round-tripping and multiple unique folder routes.
- On Windows, map a folder both directions, verify the letter appears in
  Explorer, verify used and free space match the source filesystem, exercise
  read/write/seek/rename/delete with native programs, and confirm each one-way
  relationship appears on both machines.
- On macOS, verify the selected label under `~/AllMyStuff Drives`, reconnect
  after source sleep, and exercise the same native file operations.
- On an Ubuntu file server, install and run the system service, verify the
  mount under `/mnt/allmystuff`, reboot both sides independently, and confirm
  the owned-mount cleanup restores the relationship without leaving a ghost
  mount.
- Take either endpoint offline, remove the relationship from the other, bring
  it back, and confirm the pending removal clears the native drive and cannot
  be resurrected by reconnect.
- Repeat over forced TURN and while disconnecting mid-read.
- Verify fleet, Files-share, and CECSupport-consent admission separately.
- Verify ISO boot, raw USB installer boot, and firmware/BIOS media on both
  NanoKVM and NanoKVM-Pro; confirm the attached computer never appears as an
  eligible source and that Eject removes the presence relationship.
