# Mapped-drive platform and transfer verification

## Release/platform matrix

CI covers the shared node and desktop backend on all five native release
targets: Windows x64, macOS ARM64/x64, and Linux ARM64/x64 (Ubuntu 22.04 floor).
It exercises unit tests and loopback SMB negotiation, not an actual OS mapping.
Use the following matrix for manual runtime acceptance; record the exact build,
OS version, package type, and CPU for both endpoints. A passing build does not
mark a runtime row as passed.

| Source | Windows receiver (SMB) | macOS receiver (WebDAV) | Linux receiver (rclone/FUSE) |
| --- | --- | --- | --- |
| Windows | Manual | Manual | Manual |
| macOS | Manual | Manual | Manual |
| Linux | Manual | Manual | Manual |

For each receiving OS, cover both shipped CPU architectures where applicable.
On Linux, include a `.deb` installation, an RPM installation on an RPM-based
distribution, and AppImage/portable installation. Debian/RPM package managers
must install `rclone` and `fuse3`; AppImage/portable users must have the helpers
and `/dev/fuse` access. Check the missing-helper failure instructions too.
Use a disposable subfolder for native read/write/seek/rename/delete, reconnect,
revocation, remote capacity where supported, and a >4 GiB round trip with hash
comparison. WebDAV representations have separate limits; record any failure
instead of inferring that a passing Windows SMB transfer proves those paths.
Clean each pair's test copies before moving to the next pair.

Windows 10, Windows 11 before 24H2, and a non-elevated receiver without the AMS
service must return a clear compatibility/privilege error before migrating an
existing Cloud Files mapped-drive alias. iOS/Android do not provide native drive
mounts. Mobile Files Browser/Fleetfiles testing is a separate transfer surface;
the Android shell-only CI build does not verify the embedded node engine.

## Windows SMB acceptance

Use a build containing this change on both the source and receiver. Windows
receivers need Windows 11 24H2 / Server 2025 or later, the installed AMS service
or an elevated node, and a signed-in user. Source folders can be hosted on
Windows, macOS, or Linux. No Windows share, local account, driver, firewall
rule, WinFsp, or rclone installation is needed.

1. Record the existing Z: mapping and its contents. Keep Z: mapped to its
   existing SMB share throughout the test. Record any other occupied letters.
2. In AMS, map an authorized source folder with **Auto**. Expect Y: when Z:
   is occupied and Y: is free; otherwise expect the next free letter. An
   explicitly requested occupied letter must also be preserved. Check both
   endpoints display the selected letter. Explorer should show a network drive.
   Repeat with an existing disconnected mapping still assigned to its letter
   (do not unmap it); AMS must skip it even though its server is unreachable.
   No letter is generically reserved, and stale AMS bookkeeping must not hold
   an otherwise free letter. Lower-letter exhaustion is covered automatically;
   do not remove real mappings or volumes to manufacture that case manually.
3. Compare its capacity with the actual source volume. It should report the
   source filesystem, rather than the receiver's C: capacity.
4. In a new disposable test subfolder, copy a file larger than 4 GiB in each
   direction. Compare source and destination lengths and `Get-FileHash` SHA-256
   values. Exercise seeking beyond 4 GiB with a suitable application. Keep the
   original source file intact.
5. Create, edit, rename, and delete small files and nested directories in that
   disposable subfolder. Verify changes at the source, including external
   source edits becoming visible in the mapped drive. Check that sibling
   folders outside the granted root remain inaccessible.
6. Disconnect/reconnect the source or restart AMS. I/O should fail while the
   source is unreachable and recover through the saved mapping relationship.
   Z: must remain intact. Explicit unmap or share revocation should close the
   private SMB endpoint and remove only this AMS mapping.
7. Open Fleetfiles separately. Its Cloud Files placeholders, hydration, and
   synchronization should still work. It should not become an SMB drive.
8. For a prior Cloud Files mapped folder, verify unsent journal entries block
   migration with the recovery path shown. After recovering those edits, verify
   the old drive alias/provider registration is removed and SMB is used. Old
   cache directories and journals must remain available for recovery.
9. Remove only the disposable test copies and test subfolder. Check Z: again.

Automated checks cover SMB negotiation, required signing, encryption
capability, private endpoint lifecycle, credential redaction, native endpoint
ownership, and the existing occupied-letter selection behavior. The full
Windows mapping and two-machine transfer checks above are manual.
