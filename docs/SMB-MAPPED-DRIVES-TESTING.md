# Windows SMB mapped-drive manual verification

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

Local automated checks cover SMB negotiation, required signing, encryption
capability, private endpoint lifecycle, credential redaction, native endpoint
ownership, and the existing occupied-letter selection behavior. The full
Windows mapping and two-machine transfer checks above are manual.
