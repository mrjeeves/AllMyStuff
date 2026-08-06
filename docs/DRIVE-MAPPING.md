# Native drive mapping over the mesh

## Decision

AllMyStuff maps an explicitly selected folder as a real operating-system
drive. On Windows the receiver sees an ordinary drive letter in Explorer and
every native application; it is not an AllMyStuff-only file browser.

The first native adapter is Windows WebDAV. A loopback-only WebDAV server runs
on the receiving node, translates filesystem operations into the existing
mesh file protocol, and is mounted with Windows `net use`. The WebDAV server
is never exposed to the LAN and the file bytes still travel only on the live
AllMyStuff route (direct, STUN, or TURN).

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
- The drive-letter field defaults to **Auto — next available**. A user may
  enter a particular `X:` instead. Enter, click-away, or Map Drive completes
  the form.
- Unmap tears down both the Windows drive and its mesh route.

## Architecture

```text
source machine                                      receiving machine
┌─────────────────────────────┐                    ┌─────────────────────────┐
│ selected local folder       │                    │ Windows Explorer / apps │
│ route-id → canonical root   │                    │           │ X:\         │
│             │               │                    │   net use + WebClient   │
│ scoped FileEvent host       │◄══ mesh route ═══►│ 127.0.0.1 WebDAV       │
└─────────────────────────────┘  direct/STUN/TURN  │ RemoteDavFs adapter     │
                                                   └─────────────────────────┘
```

One mapping is a unique `Storage` route:

```text
<source>:drive-map:<nonce> → <receiver>:storage-in
```

The route offer carries only a friendly label and requested receiver mount.
The absolute source path never crosses signaling. Before offering, the source
canonicalizes the chosen path and binds it locally to the unique route id.
Multiple folders between the same pair therefore remain independent.

The native adapter adds metadata and random-access read/write operations to
`FileEvent`. Replies for OS filesystem calls use a dedicated per-request
waiter instead of the GUI Files queue. Existing whole-machine Files sessions,
room shared-file downloads, and their permissions remain separate.

## Lifecycle

1. Source selects and canonicalizes a folder, binds it to a fresh route id,
   and offers the Storage route with `DriveRouteOffer` metadata.
2. The receiver accepts under the normal Files authorization gate.
3. On activation, the receiver binds an ephemeral listener to
   `127.0.0.1:0`, builds `RemoteDavFs`, chooses the next free letter (Z down to
   D when Auto), and runs `net use <letter> http://localhost:<port>/
   /persistent:no`.
4. Explorer WebDAV requests become scoped FileEvents over the active route.
5. Route teardown aborts the listener, cancels in-flight RPCs, runs `net use
   <letter> /delete /y`, and forgets the source root.
6. If native mounting fails, the receiver tears the route down instead of
   showing a live connection line for a drive Windows cannot use.

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

- Windows is first-class now because it supplies a native WebDAV redirector;
  no WinFsp/Dokan driver installation is required. macOS and Linux builds
  compile but return a clear unsupported result until their mount adapters
  (`mount_webdav`, GVfs, or an equivalent) are implemented.
- WebDAV favors zero-install interoperability over perfect POSIX semantics.
  Windows applications get ordinary read/write/seek/rename/delete behavior;
  filesystem features WebDAV cannot represent (hard links, alternate streams,
  POSIX ownership) are outside this contract.
- Remote-source selection uses the already-authorized mesh Files session.
  Pretending a native dialog on this PC could browse another PC was rejected;
  so was temporarily exposing an entire remote disk merely to feed a picker.
- The receiver chooses Auto at activation because only it knows which drive
  letters are actually free. The route metadata may continue to say Auto on
  the source; the receiver's OS remains authoritative.

## Verification

- Unit-test scoped paths, symlink/parent escape rejection, metadata, ranged
  reads, and ranged writes.
- Test offer metadata round-tripping and multiple unique folder routes.
- On Windows, map a folder both directions, verify the letter appears in
  Explorer, read/write/seek/rename/delete with native programs, and confirm
  Unmap removes it.
- Repeat over forced TURN and while disconnecting mid-read.
- Verify fleet, Files-share, and CECSupport-consent admission separately.
- Verify ISO boot, raw USB installer boot, and firmware/BIOS media on both
  NanoKVM and NanoKVM-Pro; confirm the attached computer never appears as an
  eligible source and that Eject removes the presence relationship.
