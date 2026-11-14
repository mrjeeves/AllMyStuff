# Native drive mapping over the mesh

This is the implementation specification. For ordinary mapping steps, start
with [Files and drives](USING-ALLMYSTUFF.md#files-and-drives).

## Decision

Mapped/shared folders use SMB3 over the authenticated AMS Storage route on
Windows, macOS and Linux. The source runs the same MIT-licensed `smb-server`
backend, confined to the granted folder. The receiver uses its native OS SMB
client through a private loopback tunnel. No listener is exposed to the LAN.
There is no WebDAV/WebClient size ceiling or mapped-folder sync cache in this
path, and no WinFsp or rclone dependency. Fleetfiles remains separate: Windows
keeps Cloud Files and macOS keeps its existing local Fleetfiles presentation.

## Distribution compatibility

| Receiving platform | Native client | Requirements |
| --- | --- | --- |
| Windows x64 | Built-in SMB redirector | Windows 11 24H2 / Server 2025 or later; AMS service or elevated node; signed-in user |
| macOS ARM64 / x64 | Built-in NetFS/SMB filesystem | Desktop user session; native mount authorization |
| Linux ARM64 / x64 | Kernel CIFS/SMB3 filesystem | Kernel CIFS support; desktop user session; polkit/pkexec mount authorization or root |
| iOS / Android | No native drive mount | Mobile remains a file browser/controller; no desktop mount is promised |

Every desktop source/receiver OS pair uses the same SMB transport. Both
endpoints need an AMS build advertising `smb-drives-v1`. Alternate SMB ports
impose the Windows receiver minimum above; OS/token preflight runs before
migration can remove a Cloud Files drive alias. Older Windows versions receive
an explicit compatibility error rather than an implicit fallback.

Linux releases retain the Ubuntu 22.04 / glibc 2.35 build floor. Debian packages
declare `pkexec | policykit-1`; RPM packages declare `polkit`. AppImage/portable
users need the host's polkit/pkexec package for an ordinary user mount. The
desktop's polkit authentication agent must be running; helper stdin is reserved
for the private SMB request, so no terminal-password fallback is enabled. The
installer attempts to install `pkexec` on apt-based systems and gives
instructions elsewhere. The client calls the kernel mount API directly; it
neither bundles cifs-utils nor installs a filesystem driver. A hardened system
service's private mount namespace cannot expose desktop mounts, so that case
fails with instructions to use the desktop user node. Service hardening stays
intact. Polkit authorizes a one-shot operation in the existing node executable;
AMS does not install a setuid helper or privileged mount daemon.

PR CI builds/tests the node and desktop backend on every native release runner:
Ubuntu 22.04 x64/ARM64, macOS 15 ARM64/x64, and Windows x64. Ubuntu 24.04 also
checks the optional FFmpeg encoder; latest Ubuntu checks the GUI. CI and release
share the Linux dependency action. Node/desktop/mobile locks require rustix
1.1.6 or later for the SMB Linux extended-attribute APIs. Locked checks prevent
a newer local resolution from hiding an incompatible committed lockfile.

Unix native-client tests mount a private share, read/write sparse data beyond
4 GiB, exercise mutation, and remove the mount and fixtures. They run with
mount privileges on Linux and through NetFS as an ordinary macOS user. Mac CI
prepares an empty, user-owned mount directory under `/Volumes` to avoid an
unattended consent dialog; the application allows NetFS's normal authorization
UI for custom locations. They do not replace real
two-machine/package tests in the [platform matrix](SMB-MAPPED-DRIVES-TESTING.md).
Windows Explorer mapping, every Linux distro, and mobile's embedded engine still
need runtime acceptance; Android CI checks only the shell without that engine.

## Licensing

AMS integration code remains MIT. `smb-server` is pinned to MIT-licensed 0.10.0;
its desktop dependency closure uses permissive licenses. MIT is selected where
available; BSD, Unicode and Zlib notices retain their own terms. This does not
relicense dependencies or OS filesystem implementations. No Samba server code
or commercial filesystem SDK is embedded. `scripts/check-smb-licenses.py`
checks each target's dependency closure in CI and release. The combined
[upstream notices](THIRD-PARTY-SMB-NOTICES.txt) ship in GUI bundles and portable
archives. A license/dependency change fails that guard until reviewed.
The node and desktop shell also embed the notices; `allmystuff-serve
--smb-license-notices` prints them even after a binary-only self-update.

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

One mapping has a stable mapping ID and Storage route:

```text
<source>:drive-map:<mapping-id> -> <receiver>:storage-in

granted folder -> private SMB3 server <== authenticated AMS tunnel ==> native OS SMB client
```

The source canonicalizes the root and binds it locally to that route. Scoped
Files RPC negotiates the per-route SMB endpoint with random credentials. The
capability-confined filesystem rejects traversal and symlink escapes. Signing
and SMB3 encryption are required; SMB leases are disabled so external source
edits remain visible. Credentials stay out of snapshots, debug output, saved
leases and process arguments. General Sites grants do not expose this endpoint:
only the route's authenticated peer can tunnel to its recorded SMB port.

Windows uses nonpersistent `New-SmbGlobalMapping`, restricted to the signed-in
account and LocalSystem. macOS uses `NetFSMountURLSync` with separate in-memory
credentials, loopback permission, native authorization UI and an exact mount
directory. Linux uses the
kernel CIFS mount with SMB3, signing, encryption, private file permissions and
no userspace write cache. Credentials pass to the one-shot helper through
stdin and then directly to the kernel. Saved leases contain only endpoint,
owner and mount identity. Linux kernel mount IDs and macOS filesystem IDs
protect replacement mounts; generation checks protect stale lease owners.

## Lifecycle

1. The source selects/canonicalizes a folder, binds it to a new route ID and
   offers Storage metadata. The receiver applies normal Files authorization.
2. The receiver checks native support and migration recovery before choosing
   its destination. Windows Auto searches Z through A with no generic letter
   reservations. Assigned, disconnected and remembered drives are occupied;
   selection never unmaps a drive to free a letter. Unix rejects an existing
   mount or nonempty destination and chooses an available directory.
3. The receiver negotiates the private SMB endpoint and starts its loopback
   tunnel. It records recovery ownership before invoking the OS mount client,
   then confirms the OS-reported mount. Both interfaces display the actual
   letter/path chosen by the receiver.
4. SMB bytes travel in bounded SiteFrames over the active Storage route. A
   failed native mount tears down its tunnel rather than reporting a usable
   drive. No native listener opens on the LAN.
5. Route loss/removal closes the SMB server and its tunnel. Cleanup removes
   only an owned endpoint/instance. A replacement drive or Unix mount is
   preserved. Recovery leases survive incomplete cleanup for the next startup.
6. Relationships retain reconnect intent; explicit removal records durable
   removal on both endpoints and retries when a sleeping peer returns.
7. Windows Cloud Files mapped-drive journals with unsent edits block migration;
   old cache directories remain. Linux blocks migration if its prior rclone
   cache contains files requiring recovery. Legacy exact-mount cleanup remains
   available; migration never deletes old cached data.
8. The separate Fleetfiles sync namespace/provider remains unchanged.

## KVM install and firmware media

KVM media is deliberately not the desktop SMB mapping described above.
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
- SMB binds only to loopback. Per-route credentials require signing/encryption
  and stay out of process arguments, snapshots and saved leases.
- Native operating-system drive mapping never depends on a KVM and never
  creates a Files share *to* a KVM. KVM virtual media is a separate,
  purpose-built path that terminates at the KVM's USB gadget.

## Compatibility and verification

Mapped drives need the source online. Interrupted operations fail rather than
becoming offline sync edits. Source permissions, capacity and filesystem limits
still apply. Native SMB mounting follows OS authorization policy; no per-route
`/etc/fstab` entry or relaxed service sandbox is installed. POSIX ownership,
hard links and every source's metadata are not promised by a passing transfer.

Unit tests cover route admission, confined paths, protocol negotiation, drive
allocation and mount ownership. Unix native-client tests exercise sparse data
past 4 GiB and cleanup. Runtime acceptance must still cover every source/receiver
pair, native application operations, reconnect/revocation, forced TURN and
interruption. Use the [manual checklist](SMB-MAPPED-DRIVES-TESTING.md). Verify
KVM ISO/raw USB/firmware media separately on NanoKVM and NanoKVM-Pro.
