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

Choose **Remote** on a device card. The console lists the screens that device
has shared with you.

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
usable Files space.

The implementation and traffic budgets are documented in
[Fleetfiles current implementation](FLEETFILES-IMPLEMENTATION.md).

Files also browses explicit remote-machine grants. Use those device/local-copy
surfaces to inspect, preview, upload, download, rename, or remove files on a
machine that has granted Files access.

**Drives** creates an operating-system mount. The result is a real drive letter
in Windows, a mounted volume on macOS, or a filesystem mount point on Linux.
Native applications use it like other storage on that computer.

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
details. Ubuntu and Debian installations use `davfs2`; the `.deb` package and
the AllMyStuff installer include it as a drive-mapping dependency.

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
the CEC Support area and help queue. A support session grants only the approved
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
