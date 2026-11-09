# Remote Access

Remote desktop for Windows computers on a local network, such as unattended work PCs.

- `rdagent.exe` runs as a service on each supervised computer and starts at boot.
- `rdviewer.exe` runs on your machine: a dashboard, with each remote desktop in its own window.

## Build

Needs Rust with the MSVC toolchain.

```
cargo build --release
```

Both programs end up in `target\release`, and neither needs anything else installed.

## Set up

1. Run `rdviewer.exe`. The dashboard shows an install command containing your admin key.
2. Copy `rdagent.exe` to the computer and run that command from an elevated prompt:
   ```
   rdagent.exe install --admin-key <key>
   ```
   Note the fingerprint it prints.
3. In the dashboard, click **Scan network** or add the computer by IP address. Check that
   the fingerprint matches, then click **Trust and add**.
4. Click **Connect**.

Run `install` again with a new build of `rdagent.exe` to update a computer. Keys and
identity are kept. To remove everything, run `rdagent.exe uninstall` from an elevated prompt.

## Using a session

| Action | How |
| --- | --- |
| Ctrl+Alt+Del | Session menu (Windows always handles the physical keys locally) |
| Full screen | Ctrl+Alt+Enter |
| Switch display | Monitor menu |
| Trade sharpness for speed | Quality menu |
| Turn clipboard sharing off | Session menu |

While the window has focus, all other keys go to the remote computer, including Win,
Alt+Tab and Alt+F4. Sessions reconnect by themselves after a reboot or network drop. A
new connection to a computer replaces the previous one.

## What gets installed

| Item | Location |
| --- | --- |
| Program | `%ProgramFiles%\RemoteAccess` |
| Service | `RemoteAccessAgent`, automatic start, restarts on failure |
| Certificate, authorized keys, connection log | `%ProgramData%\RemoteAccess`, SYSTEM and Administrators only |
| Firewall | Inbound TCP 47800 and UDP 47801, from the local subnet only |
| Policy | `SoftwareSASGeneration=1`, which lets the service send Ctrl+Alt+Del |

Each computer's connection history is in `connections.log`. Nothing appears on the
computer's own screen during a session, so its work isn't interrupted, but
whatever you do remotely is shown there.

## Security

- All traffic uses TLS 1.3. The viewer pins each computer's certificate fingerprint and
  refuses the connection if it changes.
- Login is an Ed25519 signature bound to that certificate. Agents store only public
  keys, so a compromised computer can't be used to reach the others.
- The private key is in `%APPDATA%\RemoteAccess\viewer.key`, encrypted with DPAPI to your
  Windows account. It controls every computer that trusts it, so protect that account.
  If you lose it, re-run `install` on each computer with the new key. Old keys stay
  listed in `%ProgramData%\RemoteAccess\authorized_keys` until you delete them there.
- The helper process runs as SYSTEM, because that's the only way to reach the lock screen.
- Don't forward these ports to the internet. It's designed for a LAN.

## How it works

The service runs in Session 0, which has no access to the desktop. After a login it starts
`rdagent helper` in the console session, connected by a named pipe open only to SYSTEM.
The helper captures with DXGI Desktop Duplication (GDI as fallback), sends changed regions
as JPEG, and injects input with `SendInput`. It follows the switch to the secure desktop
for the lock screen and UAC.

## Testing

```
cargo test --workspace
cargo test --release -p rd-agent -- --ignored --nocapture   # needs a real display
```

To run without installing or admin rights, get your key with `rdviewer key`, then:

```
rdagent console --port 47900 --data-dir <dir> --admin-key <key>
rdviewer snapshot --addr 127.0.0.1:47900 --seconds 5
```

Console mode can't see the lock screen or UAC prompts, or send Ctrl+Alt+Del.

## Known limits

- Encoding is JPEG, not video. Office and desktop use is smooth, but full-screen video
  on a 4K display is limited to around 15-20 fps. Frame rate is capped at 30 to protect
  whatever the computer is showing.
- Windows only, and only the physical console session, not RDP sessions.
- Text clipboard only.
- The remote pointer isn't drawn. The viewer shows its own cursor, in the remote cursor's
  shape, so you won't see someone moving the mouse at the computer itself.
- Scaling is fit-to-window only.
- Scan only reaches the viewer's own subnet. Elsewhere, add computers by address.

## macOS viewer

The viewer also runs on macOS (Apple Silicon and Intel, macOS 11 or later). The agent
stays Windows-only.

Install or update it with:

```
curl -fsSL https://raw.githubusercontent.com/dennik/remote_access/main/macos/install.sh | sh
```

That puts **Remote Access** in `~/Applications`, adds an `rdviewer` command, and prints
this Mac's admin key. Each Mac has its own key, stored in the login keychain, so run
`rdagent.exe install --admin-key <key>` once on each PC to authorize it. Existing keys
are kept.

On a Mac, Command and Control both act as Ctrl, so Command+C and Command+V copy and paste
on the remote computer. The Windows key and Ctrl+Alt+Del are in the Session menu, and
Ctrl+Option+Return toggles full screen. macOS keeps Command+Tab, Command+Q and
Command+Space for itself.

To build a release, run `macos/build-app.sh` on a Mac with Rust installed and attach
`target/macos/RemoteAccess-macos.zip` to a GitHub release.
