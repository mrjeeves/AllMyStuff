# Mesh-native drive mapping

## Assumptions and requirements

- A user can map one currently mounted volume to another AllMyStuff machine or
  a connected CECSupport machine without a KVM.
- The mapping is read/write and lasts until its route is disconnected.
- Mapping a volume must not imply permission to browse the source machine's
  home directory or its other drives.
- The first implementation is app-native: the receiver opens the mapped drive
  in AllMyStuff or CECSupport. It does not install an OS filesystem driver or
  claim a Windows drive letter.

## Architecture

Inventory already produces a stable capability for each real volume. The
bridge adds one synthetic `Storage/Sink` capability per machine,
`<node>:storage-in`. A mapping is therefore an ordinary graph route:

```text
source:disk:<mount>  -- Storage route -->  receiver:storage-in
        |                                      |
        | FileEvent request/response frames    | mapped-drive browser
        +--------------------------------------+
```

The existing file plane carries list, read, write, mkdir, rename, delete, and
download traffic. Whole-machine Files continues to use a generic `:files`
route and its owner/fleet/share/CEC control gate. A mapped Storage route is a
separate explicit lease scoped to the source capability's volume.

## Contract and lifecycle

1. The source UI selects a local `origin=storage` capability and the target's
   `origin=storage-in` capability.
2. Normal route offer/accept establishes the Storage route over the mesh and
   therefore works over direct, STUN, or TURN paths.
3. On the source, route activation resolves the capability against a fresh
   inventory scan and binds the route id to that recorded mount point.
4. On the receiver, activation creates the file-response queue. The browser
   sends the existing `FileEvent` messages on the Storage route.
5. Disconnect/unmap removes the route, cancels in-flight reads, clears the
   response queue, and forgets the bound root.

CECSupport drives the same node-control commands. Either side may be the source;
an incoming drive is openable in its Drive mapping card, while an outgoing one
is shown with an Unmap action.

## Security

- The root comes only from local inventory; a peer cannot name an arbitrary
  host path.
- Viewer paths are virtual `/` paths. Parent traversal and platform prefixes
  are rejected.
- The nearest existing ancestor and existing targets are canonicalized, which
  prevents symlinks from escaping the mapped root.
- The virtual root cannot itself be renamed or deleted.
- A peer cannot fabricate an inbound mapping that sources this machine: mapped
  Storage offers are classified as the Files drive plane and pass the normal
  privileged-offer gate. A locally initiated mapping is the explicit grant.
- KVM nodes are excluded as destinations in the UI.

## Failure modes

- If the drive is unplugged, operations return that the mapped drive is
  unavailable; Unmap remains available from the live route.
- If the receiver is on an older build without `storage-in`, the source asks
  the user to update it instead of routing onto a physical target disk.
- A dropped mesh/TURN path follows normal route teardown and reconnect rules;
  file reads are bounded and cancellable, so a broken receiver cannot grow
  source memory without limit.

## Alternatives considered

- Reusing whole-machine Files was rejected because it grants materially more
  access than mapping a plugged-in volume.
- A bespoke transfer channel was rejected because the tested file protocol
  already supplies streaming, backpressure, uploads, and downloads.
- OS drive letters/FUSE/WinFSP/WebDAV were deferred: each adds platform
  services or drivers and a second local filesystem server. The mesh contract
  deliberately stays independent so such an adapter can be added later.

## Testing and implementation steps

- Unit-test scoped listing/reading and traversal refusal in `node/files.rs`.
- Unit-test the dedicated route shape and privileged-plane classification.
- Run graph/bridge tests, node check, both Svelte checks/builds, and the
  CECSupport Tauri check.
- Validate end-to-end with two current nodes: map a removable volume in both
  directions, force TURN, upload/download/rename/delete, unplug it mid-session,
  and confirm Unmap tears down access immediately.
