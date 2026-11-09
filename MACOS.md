# macOS agent: status and next steps

The macOS agent (`rdagent` on a Mac) lets the Windows viewer connect to Macs. It was
written on Windows. It type-checks for Apple Silicon and Intel Macs, but it has **not been
built, linked or run on a Mac yet**. Start here when working on a Mac.

## What's done

| Part | File | Status |
| --- | --- | --- |
| Screen capture (CGDisplayCreateImage) | `crates/agent/src/platform/mac/capture.rs` | Type-checks |
| Mouse and keyboard (CGEvent) | `crates/agent/src/platform/mac/input.rs` | Type-checks |
| PC scan code to Mac key code map | `crates/agent/src/keymap.rs` | Unit-tested on Windows |
| Permission prompts | `crates/agent/src/platform/mac/permissions.rs` | Type-checks |
| Installer (launchd agent at login) | `crates/agent/src/platform/mac/install.rs` | Type-checks |
| Clipboard (pbcopy, pbpaste) | `crates/common/src/clipboard.rs` | Type-checks |
| Network, login, session, encoding | shared with Windows | Tested on Windows |

The protocol hasn't changed, so the current Windows viewer works with Mac agents as is.

## How it differs from Windows

- **No service, no helper.** A launchd agent (`/Library/LaunchAgents/com.remoteaccess.agent.plist`)
  runs `rdagent run` in the logged-in user's session and does everything in one process.
- **Only while a user is logged in.** The login window and lock screen can't be captured
  or controlled this way. For unattended Macs, turn on automatic login.
- **Permissions:** Screen Recording and Accessibility, granted by hand in System Settings.
- **Files:** the program and `authorized_keys` are in `/Library/Application Support/RemoteAccess`,
  owned by root. The identity and `connections.log` are in
  `~/Library/Application Support/RemoteAccess`, readable only by that user.
- **Keyboard:** keys map by position. The Windows key is Command and Alt is Option, so
  Win+C copies, Win+V pastes and Win+Tab switches apps. Send Ctrl+Alt+Del does nothing.
- **Pointer shape:** always an arrow in the viewer, because reading it needs AppKit.

## Build and try it

```
xcode-select --install          # if not already installed
cargo test --workspace          # the viewer is skipped on macOS
cargo build --release -p rd-agent
sudo ./target/release/rdagent install --admin-key <key from install-command.txt>
```

Then grant both permissions as the installer explains, restart the agent, and add the
Mac in the Windows dashboard.

For a quick test without installing, run this as the logged-in user:

```
./target/release/rdagent console --bind 0.0.0.0 --data-dir /tmp/rdtest --admin-key <key>
```

## Test checklist, most likely to need fixes first

1. **It builds and links.** Only type-checking has been done.
2. **Capture shows windows, not just the wallpaper.** Wallpaper only means Screen Recording
   isn't granted. If CGDisplayCreateImage fails on a newer macOS, replace `DisplayImage`
   with ScreenCaptureKit (`SCStream`). Keep the `Capturer` trait so nothing else changes.
3. **Colours are right.** Red and blue swapped means the image isn't BGRA. Check the image's
   bitmap info and swap channels in `grab`.
4. **The pointer lands where clicked**, on Retina and on a second display. Positions are in
   points, captures are in pixels.
5. **Double-click, drag, right-click and scrolling.** Check that horizontal scroll goes the
   right way. Its sign is a guess, in `wheel`.
6. **Shortcuts:** Win+C and Win+V, Shift, and Option characters.
7. **The installer:** reinstalling over an existing install, uninstalling, and the agent
   starting after a reboot with automatic login.
8. **CPU use.** Capture runs at up to 30 fps without change notifications, so check load on
   a 5K display. ScreenCaptureKit would also fix this.

## Troubleshooting

**The screen shows but the mouse and keyboard do nothing.** The agent is missing the
Accessibility permission. macOS silently discards its input, and neither end reports an
error. In System Settings → Privacy & Security → Accessibility:

- An installed agent needs the entry for `/Library/Application Support/RemoteAccess/rdagent`.
  A build in `target/release` doesn't count.
- An agent started from Terminal for testing gets its permission through Terminal, so
  Terminal needs the entry instead.
- If `rdagent` is listed and switched on but control still fails, the entry is stale from
  an earlier build. Remove it with −, add the binary again with +, and switch it on.

Then restart the agent: `sudo launchctl kickstart -k gui/$(id -u)/com.remoteaccess.agent`.
A missing permission is also logged at startup in
`~/Library/Application Support/RemoteAccess/connections.log`.

**The screen shows only the wallpaper.** The same, for the Screen Recording permission.

## Code signing

Every rebuild changes the binary's signature, and macOS then forgets the two permissions.
They have to be granted again after each update. Signing with a Developer ID certificate
(Apple Developer account) fixes this, and is needed before rolling out to many Macs.
