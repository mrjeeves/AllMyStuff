# Using AllMyStuff

AllMyStuff becomes much simpler once you choose the relationship first:

- **Fleet** for devices you own together.
- **Share** for giving another person specific access.
- **Mesh** for private reachability without granting access.
- **Room** for a shared session with several participants.
- **CEC Support** for a temporary customer and technician relationship.

Seeing a device is not the same as being allowed to use it. Every control in
the app follows one of these relationships.

## The main view

The graph is the home screen.

**Normal mode** is the everyday view. It keeps both sidebars out of the way,
groups your devices by fleet, and puts the most useful labeled actions directly
on each device.

**Advanced mode** keeps the same fleet grouping and adds the fuller device,
routing, and graph controls. Use it when you need to inspect hardware, active
connections, shares, or less common device actions.

The left sidebar is available in either mode for sites, rooms, and help. The
detail sidebar belongs to Advanced mode.

## Add a computer to your fleet

Install and open AllMyStuff on both computers while they are on the same local
network.

The Windows installer includes Microsoft's signed Visual C++ runtime package
for the mesh daemon and installs it automatically. The PowerShell installer
downloads and verifies the same prerequisite. The app and node binaries also
include their C/C++ runtime, including in portable downloads and self-updates.

1. On the computer being added, find its **This Device** card.
2. Choose **Make claimable**.
3. On a computer already in your fleet, select the device under **Ready to
   claim**.
4. Choose **Claim this device**.
5. Give it a useful name by editing its label or opening **Settings > This
   Device**.

Claim mode is temporary. A device does not remain silently claimable after it
restarts.

Once claimed, the device can leave the LAN and remain part of the fleet. A mesh
supplies reachability; the signed fleet membership supplies authorization.

### Claim a remote computer

Remote claiming uses an intentional ID exchange instead of LAN discovery.

1. On the computer being added, open **Settings > Fleet** and turn on **Allow
   claiming over the public mesh**.
2. Choose **Make claimable** on that computer's **This Device** card.
3. Return to **Settings > Fleet** and copy **This device's remote claim ID**.
4. On the computer that will own it, turn on **Allow claiming over the public
   mesh** and enter that ID under **Claim a remote device**.
5. Choose **Claim device**.

The ID is shown only while the device is actively claimable. It is spent after
a successful claim. Public claiming is a setting on each device, so enabling it
on one computer does not silently enable it anywhere else.

## Remote control

Choose **Remote** on a device card. A durable **Screens** share covers all
current and future monitors on that machine. Changing a monitor, restarting,
or moving between monitors does not require another share.

Use **Rooms** when sharing an individual monitor or app for a session. A room's
selected source does not create a durable grant.

On desktop, Remote Control opens a window once its interface is ready. Selecting
the same machine again restores its existing window or replaces an unresponsive
one. If startup fails, the main window shows the error; choose Remote Control
again to retry.

- Use the screen list to move between monitors.
- Pop a monitor into its own native window. You can pop out several monitors
  and arrange those windows across your local displays to match the remote
  desk, while the main console remains open.
- Clipboard synchronization carries text, images, and files.
- Drop a local file onto the remote desktop to transfer it.
- Use Relative Mouse for games and other programs that capture raw movement.
- Press Esc to release pointer capture.
- Hide the toolbar when you need the full remote surface.

Camera access is separate from screen sharing. Sharing remote control does not
silently share a camera.

On Linux, keyboard and mouse control supports GNOME Wayland and X11. GNOME
desktop access requires an existing, unlocked graphical session; Always On
does not provide a login screen. See [Linux remote desktop access](linux-remote-desktop.md)
for session requirements and unattended-access limits.

If the computer has an attached KVM, the console can also expose its screen,
power menu, and install-media controls when your relationship permits them.

## Files and drives

**Files** is the middle app mode and the normal home of **Fleetfiles**, your
fleet-wide logical filesystem. Fleetfiles paths do not name a computer. The app
keeps namespace and version knowledge on the fleet, places verified file bodies
on allocated storage, and retrieves only the bodies you open when they are not
already available locally.

The Navigator starts at **Fleetfiles**. Use its folder tree without thinking
about physical placement. Choose **Local copies** only when you deliberately
want to inspect one device's native working tree; that view expands Devices and
shows a **Local copies only** banner so it cannot be mistaken for the whole
fleet.

The search chevron switches between **Search this Folder** and **Search
Fleetfiles**. Fleetfiles search queries the indexed logical namespace across
folders without reading file bodies. Search results and large folders load
bounded pages as you scroll, with no result-count cutoff, and the main view
virtualizes what it renders. Recent items remain compact in the sidebar.

Right-click a logical file and choose **Version History** to see retained
versions. **Restore as current** fetches the exact verified body from an online
fleet member when necessary, then creates a new current version rather than
destroying later history. History defaults to 30 days, keeps current files
first, and uses available allocated space beyond the retention target when it
can. A dot on the Files mode button warns when the fleet is low on protected
usable Files space. Open **Files** to read the matching notice below its toolbar.
Choose **Manage storage** to review contributions and copy settings, or
**Dismiss** to hide both the notice and the navigation dot for that issue.
Dismissal is remembered on this device; a changed warning or a recurrence after
recovery can notify you again.

Fleet devices with enough space contribute **1 GB** (1 GiB) automatically on
one local disk, without opening Settings. Setup leaves at least **5 GiB or 10%
of that disk free**, whichever is larger, after the contribution. Removable,
read-only, network, and virtual adapter volumes are not enrolled automatically.
The contribution is a storage budget, not a preallocated file. Use
**Settings > Files > Fleetfiles storage** to change the budget or turn it off;
saved contributions and opt-outs take precedence, including choices that arrive
later from an offline device. Devices without room can become eligible after
space is freed. Automatic contributions are shared with upgraded fleet members;
older app versions continue to use explicit storage allocations.

On macOS, an allocation on the startup Data volume can use the app's writable
state directory on that same volume when the volume root is protected. A
failed allocation retries on later file operations with a delay that grows
from 5 seconds to at most 5 minutes. The log reports the first failure, a
changed error, and recovery rather than repeating the same warning for every
file chunk. Changing the allocation lets it try again immediately.
An older transfer finishing late cannot replace a newer committed version;
its verified content remains available as history.

The implementation and traffic budgets are documented in
[Fleetfiles current implementation](FLEETFILES-IMPLEMENTATION.md).

On macOS, disk discovery skips capacity probes on network mounts, including
the Fleetfiles Finder mount. Manual inventory scans run in the background so
other control requests can continue while hardware discovery completes.

Files also browses explicit remote-machine grants. Use those device/local-copy
surfaces to inspect, preview, upload, download, rename, or remove files on a
machine that has granted Files access.

The older **Files Browser** window also transfers directly to and from a
machine with Files access. Uploads read the selected file in small pieces,
including files larger than 4 GB, and publish it after the host acknowledges
the complete body and its length is checked. An existing file with the same
name is preserved; rename it or choose a different destination first. Cancel
an upload from its transfer row to discard the temporary body. Keep both
machines awake and the session open until the transfer finishes. If a lost
connection prevents temporary-file cleanup, the failed transfer shows its
path so it can be removed after reconnecting.

Downloads stream straight to the receiving machine's Downloads folder.
They require enough free space there; previews in the window have a separate
10 MB limit. Fleetfiles transfers also require sufficient allocated Fleetfiles
storage on the receiving fleet device.

**Drives** creates an operating-system mount. The result is a real drive letter
in Windows, a mounted volume on macOS, or a filesystem mount point on Linux.
Native applications use it like other storage on that computer.

On Windows, mapped and shared folders use the built-in SMB client over AMS.
They appear as network drives, support files larger than 4 GB, and report the
source volume's capacity. Reads and writes go to the source over the mesh;
keep the source online while using its drive. No additional filesystem driver,
WinFsp, or rclone is required. The receiving computer needs Windows 11 24H2 or
Windows Server 2025 or later and the installed AMS service (or an elevated node).
Both endpoints must run an AMS version with SMB drive support. Fleetfiles
continues to use Cloud Files for its separate on-demand synchronized namespace.

Auto selects the next free drive letter from Z downward. If Z: already belongs
to an SMB share, AMS preserves it and uses Y: when that letter is free. An
occupied requested letter is also preserved. Previous Cloud Files mapped-drive
caches are retained; unsent edits block migration until they are recovered.

To map something from another device:

1. Choose **Drives** on the receiving device.
2. Choose **Map new Drive**.
3. Select an authorized source device.
4. Browse to the shared folder or drive.
5. Keep the suggested drive letter or mount point, or choose another available
   destination.
6. Confirm the mapping.

A drive mapping is one-way. Both affected devices show the same relationship,
name, native destination, and availability, but a reverse mapping is a
separate choice.

If the source sleeps or changes networks, the mapping becomes unavailable and
reconnects when the source returns. Removing a mapping removes the relationship
from both devices and clears the native mount. See
[Native drive mapping](DRIVE-MAPPING.md) for implementation and compatibility
details. Ubuntu and Debian installations use `rclone` and FUSE; the `.deb`
package and the AllMyStuff installer include `rclone` and `fuse3` as mapping
dependencies. Desktop mappings run as your signed-in user, without `/etc/fstab`
entries. A restricted system service must hand desktop work to a user node;
its sandbox is not relaxed to allow mounting.

## Share with another person

Sharing is directional. **Shared by you** and **Shared with you** are different
lists.

1. Open **Settings > Sharing**, or choose **Share** on an eligible device.
2. Select the person or fleet.
3. Choose only the capabilities they should receive.
4. Add the folders, drives, screens, cameras, microphones, or sites that belong
   in the grant.
5. Save the share.

Sharing your screen or files does not grant you access to theirs. If both sides
want access, each side creates its own share.

The recipient sees the shared mount or capability, not an unrestricted view of
the source machine.

## Meshes

A mesh answers one question: can these peers reach each other privately?

It does not answer who owns a device or what anyone may do with it. Those
decisions come from fleet membership, shares, rooms, and support consent.

Open **Settings > Meshes** to join, enable, disable, leave, or inspect a mesh.
The join form is always available. A disabled mesh can still be left.

## Rooms

Rooms are for collaboration, not permanent ownership.

Create or join a room, then explicitly offer the screens, control, cameras,
microphones, chat, or files needed for that session. Room access stays scoped
to the room.

For a private conversation with one other device, use **Room** on its graph
card. AllMyStuff reuses the existing private room for that pair when possible.

## KVMs

A KVM remains useful when the attached computer is off, rebooting, in firmware,
or unable to run AllMyStuff.

- **Remote** opens KVM video and input.
- **Power** offers wake, short press, long press, and reset actions.
- **Drives** presents ISO, IMG, or removable USB media to the attached computer.
- **Settings** contains Wi-Fi, sites, attachment, update, and device controls.

Install media comes from an authorized fleet, shared, or support machine. The
attached computer cannot also be the source because it is the destination that
will reboot into the media.

## CEC Support

CEC Support is deliberately separate from the ordinary device graph.

Customers use the CEC Support app or a CEC KVM to ask for help. Technicians use
the CEC Support directory and customer session rooms. A support session grants only the approved
capabilities and should not turn either computer into a permanent fleet member
or graph fixture.

Use the customer number field to open a known customer directly. Remove stale
support relationships from the row action when they are no longer needed.

## This device and settings

Settings opens to **This Device**, where you can:

- Rename the local device.
- Review its operating system, hardware, version, and identity.
- See active connections.
- Rescan the machine.
- Open a terminal.
- Restart AllMyStuff or the device.

The remaining tabs separate fleet management, sharing, meshes, remembered
devices, CEC Support, Always On, updates, and destructive recovery actions.

## Headless machines

Run a machine without the desktop interface:

```sh
allmystuff serve
```

Keep it running across logins and reboots:

```sh
allmystuff service install
```

Use `amst <machine>` from another fleet device to open a terminal on it.

## When something looks stale

Start with the smallest relevant refresh:

1. Use the device card's **Refresh** action to refresh its network state.
2. The **Refresh** action on the **This Device** graph card also rescans the
   local machine.
3. Open **Settings > Updates** and compare the installed and pinned component
   versions.
4. Use the repair action only for the component that is out of sync.
5. If the device itself is asleep or offline, restore it before removing and
   recreating relationships.

Do not solve an authorization problem by adding more meshes. First identify
whether the device should be fleet-owned, shared, in a room, or in a support
session.

### Support-number approval

Connect to a new support customer by entering their nine-digit support number.
Incoming requests on this computer appear in the main window with the technician's
name and verification code. Approve once, approve for three hours, or decline.
Existing standing-grant controls remain in CEC settings.

Choose **# Support** on this computer or a PC or KVM in your fleet, in Normal
or Advanced mode (also available in the device details drawer). The dialog
shows **that device's number**. Read it to your technician, approve or decline
a pending request, or choose **Approve current or next request**. With no
request pending this opens a five-minute wait for one request. Pressing again
refreshes that wait; one request consumes it and receives three hours of
access. Older PCs need an AllMyStuff update to manage their approvals remotely.

Support runs in the AllMyStuff node. The standalone support app is a thin
client of that same service: discovery, requests, consent, and sessions do not
require its window to be open. Managing another PC's approvals requires owner
or fleet authority; a technician's support grant or an ordinary share does not
give permission to approve further access. The KVM web UI, support card, and
physical KVM button continue to use the appliance's shared approval window.
