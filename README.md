# Remote Access

Remote desktop for computers on a local network, such as unattended work PCs.

## What it can do

- **Full remote control** of Windows PCs and Macs: live screen, mouse and keyboard,
  every display, and a shared text clipboard. Works from a Windows PC or a Mac, in any
  direction.
- **No Windows Remote Desktop needed.** The agent doesn't use RDP, so it works on
  Windows Home editions, which can't be controlled with Remote Desktop, and on any PC
  where remote access was never set up. Install one program and that's it.
- **Login screen, lock screen and UAC prompts** on Windows, including Ctrl+Alt+Del, so
  you can sign in after a restart.
- **File transfer both ways.** Drop files on a session window and choose a folder on
  the remote computer, or browse the remote computer and copy a file back. Existing
  files are never overwritten.
- **Wake-on-LAN.** The dashboard learns each computer's network card address and can
  wake it from sleep over a wired connection. Connecting to a sleeping computer keeps
  sending wake signals until it answers.
- **Stays reachable.** Windows agents run as a service from boot. Mac agents keep the
  Mac from sleeping while idle and turn its display on when you connect.
- **Unattended.** Nothing appears on the remote screen, so the computer's own work isn't interrupted.
- **Dashboard** with online status, network scan, renaming, and one-click install
  commands.
- **Secure by default:** TLS 1.3, pinned certificates, and per-viewer Ed25519 keys.

- `rdagent` runs on each supervised computer. On Windows it's a service that starts at
  boot. On macOS it starts when the user logs in. See [MACOS.md](MACOS.md) for the macOS agent.
- `rdviewer` runs on your Windows PC or Mac: a dashboard, with each remote desktop in its own window.

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

## Set up a Mac

On the Mac, with the user who should be visible logged in:

```
cargo build --release -p rd-agent
sudo ./target/release/rdagent install --admin-key <key>
```

Then turn on **Screen Recording** and **Accessibility** for `rdagent` in System Settings →
Privacy & Security, as the installer explains. Add the Mac in the dashboard as usual. On a
Mac, the Windows key acts as Command. Details and limits are in [MACOS.md](MACOS.md).

## Using a session

| Action | How |
| --- | --- |
| Ctrl+Alt+Del | Session menu (Windows always handles the physical keys locally) |
| Full screen | Ctrl+Alt+Enter |
| Switch display | Monitor menu |
| Trade sharpness for speed | Quality menu |
| Turn clipboard sharing off | Session menu |
| Send files | Drop them on the window, or Session menu → Send files… |
| Copy a file from the computer | Session menu → Get a file… |
| Wake a sleeping computer | **Wake** in the dashboard, or just **Connect** |

While the window has focus, all other keys go to the remote computer, including Win,
Alt+Tab and Alt+F4. Sessions reconnect by themselves after a reboot or network drop. A
new connection to a computer replaces the previous one.

Sent files go into the folder you pick. A file with the same name is saved as
`name (2).ext` instead of replacing it, and an interrupted transfer leaves nothing
behind. Folders can't be sent yet, only files. Transfers need the agent and viewer from
the same release or later; older agents simply don't offer it.

Wake-on-LAN needs a network cable and wake-on-network turned on (on a Mac, "Wake for
network access"; on Windows, the network adapter's power settings and often the BIOS).
The dashboard learns a computer's address the first time it sees it online.

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
- Windows agents serve only the physical console session, not
  RDP sessions. Mac agents serve only a logged-in user, not the login window.
- Text clipboard only. Files go through file transfer.
- The remote pointer isn't drawn. The viewer shows its own cursor, in the remote cursor's
  shape, so you won't see someone moving the mouse at the computer itself.
- Scaling is fit-to-window only.
- Scan only reaches the viewer's own subnet. Elsewhere, add computers by address.

## macOS viewer

The viewer also runs on macOS (Apple Silicon and Intel, macOS 11 or later), with the
same dashboard, file transfer and Wake button. Macs can also run the agent; see
[Set up a Mac](#set-up-a-mac).

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
