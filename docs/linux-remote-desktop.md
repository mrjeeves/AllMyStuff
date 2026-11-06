# Linux remote desktop access

AllMyStuff's terminal and desktop connections have different requirements.
A terminal needs the running node and an authorized mesh connection. Desktop
sharing also needs a graphical session, a working screen-capture backend, and
an input backend that can control that session.

Always On keeps the node reachable across reboots and logouts. It does not
create a graphical login session or make every Linux display server support
unattended desktop access.

## Supported desktop sessions

| Desktop session | Screen sharing | Keyboard and mouse |
|---|---|---|
| GNOME on Wayland, including GNOME 42 | Mutter screen-cast session paired with its remote-desktop session | Mutter's native remote-desktop API, associated with the same viewer and screen stream |
| Other Wayland desktops | ScreenCast portal, subject to the desktop's consent and restore-token support | Unavailable through this backend; AllMyStuff reports the limitation instead of sending events through X11 |
| X11 | Existing X11 capture path | Existing X11 input path |
| No graphical session | Unavailable | Unavailable |

The GNOME path uses the compositor's native screen and input APIs together.
It does not depend on an XWayland connection to control native Wayland
applications. AllMyStuff's fleet, sharing, room, and support-consent checks
still apply before input reaches the desktop backend.

Other Wayland desktops continue to use the ScreenCast portal for viewing.
Their first connection can require someone at the machine to select a screen.
Where the portal supports persistent grants, AllMyStuff stores and renews a
restore token. A token does not guarantee that the compositor will restore a
grant silently, and it does not authorize keyboard or mouse injection.

## What unattended access means

On a supported GNOME desktop, AllMyStuff can reconnect to an existing
logged-in graphical session without treating each connection as a new
screen-sharing permission request. The node must run with access to that
user's session D-Bus and compositor environment. Enabling a background service
alone is not proof that those requirements are met.

This backend does not provide a remote login screen, create a desktop before
login, or bypass GNOME's screen lock. GNOME can stop or restrict screen and
input sessions when the desktop locks or the user logs out. Use a KVM when
access must continue through the login screen, firmware, or a missing desktop
session.

An X11 connection likewise needs access to the intended X server and its
authentication credentials. Running a service under another account does not
grant that access.

## When viewing works but control does not

Screen viewing and keyboard/mouse control use separate routes. If the OS
input backend cannot initialize or rejects an event, AllMyStuff ends the
control route and reports the actual error to the viewer. It releases any
inputs held by that route and leaves the screen stream available.

Check these conditions on the Linux machine:

1. A user is logged into the intended graphical desktop.
2. That desktop is GNOME Wayland or X11 when keyboard/mouse control is needed.
3. The desktop is unlocked.
4. AllMyStuff runs in, or is attached to, that same user's graphical session.
5. On a portal-based connection, the screen-selection request has been
   approved and the desktop still permits the stored grant.

For a per-user Always On installation, inspect its status and recent logs:

```sh
systemctl --user status allmystuff.service
journalctl --user -u allmystuff.service -b --no-pager -n 100
```

The error shown by the remote console distinguishes unavailable input from
an authorization refusal. A working terminal connection confirms that the
node and mesh route are available; it does not confirm desktop capture or
input access.

## Validation

Native Linux compilation and unit tests check API compatibility, route
rejection, and input translation. A desktop smoke test must additionally
verify screen capture, pointer motion, clicks, wheel events, typing, and
modifier release in a native Wayland application. Test reconnects and the
Always On service separately from a node started in a graphical terminal.

Lock, logout, and reboot tests must preserve the limits above: the app should
report an unavailable desktop or input backend while terminal access remains
usable where its service is running. WSL and headless CI cannot establish
GNOME compositor or login-screen behavior.
