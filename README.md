# crossdesk

Use one mouse and keyboard across computers on your local network (Windows and macOS).
Push the cursor past a screen edge and it continues on the other computer, as if that
computer's screen were one more monitor. The keyboard follows the cursor, and copied
text follows too.

## How it works

- **Discovery**: every copy announces itself over UDP broadcast (port 47801), so the
  computers find each other on their own. If the network blocks broadcast, pair by IP.
- **Pairing**: click **Pair**. The other computer shows a 6-digit code, and you type it
  on the first one. The code goes through SPAKE2, so it never crosses the network, and
  both sides keep a key from it. Later connections are automatic, and all traffic
  (TCP 47800) is encrypted with ChaCha20-Poly1305.
- **Screens**: the *Screens* tab shows every monitor of every connected computer on one
  map. Drag a computer's screens next to the screen they sit beside on your desk; they
  snap edge to edge. The slider sets each computer's size on the map, which also sets
  how fast the cursor crosses it. The layout is shared, so arranging it on one
  computer is enough.
- **Keys** travel as physical positions (USB HID codes), and each computer applies its
  own keyboard layout. With ABNT2 on both sides, everything matches. By default Ctrl and
  Cmd are swapped between Windows and macOS, so Ctrl+C on the PC does Cmd+C on the Mac.

Shortcuts on the computer whose keyboard you are using:

| Shortcut | Effect |
|---|---|
| Ctrl+Alt+Shift+Q | Bring the cursor back to this computer |
| Ctrl+Alt+Shift+L | Lock the cursor to the current screen (games) |

## Install

### Windows

```powershell
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-gnu
winget install BrechtSanders.WinLibs.POSIX.UCRT
.\scripts\build-windows.ps1
```

Run `target\release\crossdesk.exe`. The first time, allow it through Windows Firewall on
**private** networks. Windows does not let a normal app control windows that run as
administrator (Task Manager, installers). To control those from the Mac, run
crossdesk as administrator.

### macOS

```sh
curl https://sh.rustup.rs -sSf | sh
sh scripts/build-macos.sh
mv target/Crossdesk.app /Applications/
```

Open Crossdesk, then in **System Settings > Privacy & Security** turn it on under
**Accessibility** and **Input Monitoring**. It starts working as soon as you allow it.
Also accept the *local network* prompt.

You can also skip building on the Mac: push this repo to GitHub and the `build`
workflow builds `Crossdesk-macos.zip` and `crossdesk.exe` as downloadable artifacts.

## Development

```sh
cargo test          # layout, key map, crypto, engine and a full pair/reconnect over TCP
cargo check --target aarch64-apple-darwin   # type-check the macOS code from Windows
```

| File | What it does |
|---|---|
| `src/layout.rs` | Shared canvas, edge crossing, snapping |
| `src/engine.rs` | Where local input goes (this computer or another one) |
| `src/net.rs` | Discovery, handshake and pairing, encrypted sessions |
| `src/crypto.rs` | SPAKE2 pairing, HKDF session keys, ChaCha20-Poly1305 frames |
| `src/input/windows.rs` | Low-level hooks and SendInput |
| `src/input/macos.rs` | Quartz event tap and CGEventPost |
| `src/input/keymap.rs` | HID ⇄ Windows scancode ⇄ macOS keycode |
| `src/ui.rs` | egui window |

Settings live in `%APPDATA%\crossdesk\Crossdesk\config\config.json` on Windows and in
`~/Library/Application Support/com.crossdesk.Crossdesk/config.json` on macOS.
