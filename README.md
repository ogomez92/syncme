# SyncMe

SyncMe keeps folders in sync between your computers (Windows and Mac) and between folders or drives on the same computer. It's one portable program. It shows a tray icon on Windows or a menu bar item on macOS. It also serves an accessible web app for setup, and it announces sync activity through your screen reader (via [Prism](prism-sdk-v0.18.2/doc/index.html)) when the web app isn't in focus.

## Using it

1. Run `syncme.exe` on Windows or `SyncMe.app` / `syncme` on the Mac. The web app opens at `http://127.0.0.1:47474/`, and the tray or menu bar icon appears.
2. Start it on your other computer too. Computers on the same network or the same Tailscale network show up under **Devices → Found nearby** within a few seconds. You can also type an address under **Add a device by address**.
3. Press **Pair** on one computer, then **Accept** on the other. The request is announced by your screen reader there.
4. Press **Add folder…** and choose:
   - the folder on this computer, like `C:\Users\usuario\meow`;
   - for each device, a checkbox **Sync to NAME** and the path there, like `C:\Users\nitropc\meow`. **Browse…** lists folders on that device.
   - **Add another folder** puts a second copy on the same device, such as another drive (local-to-local sync).
5. Use **Edit…** at any time to change paths, add a device you paired later, or remove one.

The settings for a folder are shared with every device involved, so you can edit them from any of those devices.

## Safety rules

- **Nothing is deleted unless a device explicitly deletes it.** Every file carries a version vector. A deletion only applies to a copy the deleting device had already seen. Files already on a device when it joins are merged: new files spread everywhere, identical files are adopted without copying, and files with different content are both kept.
- **Conflicts keep both versions.** If a file changes on two devices before they sync, the newer one keeps the name. The other becomes `name.sync-conflict-YYYYMMDD-HHMMSS-DEVICE.ext` on every device.
- **Removed files go to the trash.** When another device deletes a file, it moves to the hidden `.syncme/trash` folder inside the synced folder. It is purged after 30 days, which you can change in Settings.
- **Missing folders are never "everything deleted".** If a synced folder disappears (drive unplugged, folder moved or recreated), that location pauses and says so. **Reset** it to merge it again safely.
- Removing a device from a folder, or stopping syncing, never deletes files.

## Screen reader announcements

Received files, deletions, conflicts, pairing requests, and devices connecting or disconnecting are announced through Prism, which supports NVDA, JAWS, Narrator (UI Automation), VoiceOver, and others. When the web app is focused, it uses its own live region instead, so nothing is read twice. Both behaviors, and a system voice fallback, are in **Settings → Announcements**.

The tray or menu bar icon shows the current state ("Up to date, 1 device connected", "Receiving report.docx from NitroPC", …). On Windows, press Win+B to reach the notification area; screen readers read the icon as "SyncMe" followed by that state. Its menu opens with Enter or a left click and also shows the latest event with how long ago it happened.

## Clipboard sync

**Settings → Clipboard** can share the clipboard between your computers. Text and images you copy on one device are put on the clipboard of every connected device that also has the setting on, in both directions. Files copied in Explorer or Finder are never sent. Text is kept exactly as copied (any language, emoji, line endings), and images travel as PNG. Texts over 4 MB and images over 24 MB or about 8K resolution are skipped. The setting is off by default and is per device: everything you copy, passwords included, is sent to those devices over your local network in the same way as your files.

## Building

Requires Rust (stable). Prism is linked statically from `prism-sdk-v0.18.2/` next to `Cargo.toml`; set `PRISM_SDK` to use another location.

- **Windows:** `cargo build --release` → `target\release\syncme.exe`. It is a single file with no DLLs or redistributables needed (the static CRT is configured in `.cargo/config.toml`).
- **macOS:** `sh scripts/build-mac.sh` → `dist/syncme` (universal) and `dist/SyncMe.app` (menu-bar only).

Portable mode: create a folder named `syncme-data` next to the program, and settings are kept there instead of in your user config folder. Other options are listed by `syncme --help`: `--data-dir`, `--port`, `--name`, `--background`, `--headless`.

## Network and security

- Devices talk over HTTP on port 47474 (TCP) and find each other with mDNS, a UDP broadcast on port 47474, and `tailscale status`. Allow SyncMe through the firewall when Windows or macOS asks.
- Only paired devices can read or write folders. Each pair shares a random secret token. Traffic is not encrypted by SyncMe itself, so on untrusted networks use Tailscale, which encrypts everything.
- The web app is only reachable from the computer itself, with host and origin checks.

## Tests

- `cargo test`: unit tests for the sync decision rules.
- `python tests/e2e.py target/debug/syncme.exe`: runs two real instances against temp folders. It covers the initial merge with pre-existing files, edits, deletes, renames, concurrent edits, offline catch-up, unplugged or recreated folders, a 64 MB file, 300-file bursts, local-to-local sync, and removing a device.
