# MapForge iOS controller

The native iPhone/iPad controller discovers MapForge Players with Bonjour on the local Wi-Fi. It connects to the Master automatically, remembers the selected Player, and offers an in-app Player picker if more than one Master is available. It does not require typing an IP address or port.

The same app has two layouts:

- **iPhone**: one scrolling column.
- **iPad**: a show-control desk. A sidebar shows the connection, the Player PCs (master and subs, online or offline) and the projector order. The main area has large Play/Pause/Stop and Blackout buttons, a live playhead, big scene tiles, and the cues and loops of the scene that is playing. In landscape, Play/Pause/Stop and Blackout sit side by side. On an upright iPad, Blackout moves under them.

The layout follows the screen width, so an iPad in a narrow Split View gets the iPhone layout.

## Files

| File | What it is |
| --- | --- |
| `ControllerModel.swift` | Discovery, polling and every command sent to the Player. No UI. |
| `Components.swift` | Shared pieces: theme colours, cards, transport buttons, scene tiles, cue buttons, lists. |
| `PhoneControllerView.swift` | The iPhone arrangement. |
| `PadControllerView.swift` | The iPad arrangement. |
| `MapForgeControllerApp.swift` | Starts the app and picks the layout. |

To change only the look, edit `Components.swift` (affects both) or one of the two layout files. `ControllerModel.swift` does not need to change.

## Install on your iPhone (then iPad)

You need a Mac with Xcode. A free Apple ID is enough for your own devices.

1. Open `MapForgeController/MapForgeController.xcodeproj` in Xcode.
2. Select the **MapForgeController** target → **Signing & Capabilities** → choose your team (your Apple ID). If Xcode says the bundle ID is taken, change it, for example to `com.yourname.MapForgeController`.
3. Connect the iPhone with a cable, select it as the run destination, and press **Run**.
4. On the iPhone, turn on **Settings → Privacy & Security → Developer Mode** if asked. Then trust your developer profile under **Settings → General → VPN & Device Management**.
5. On first launch, allow MapForge to find devices on the local network.

For the iPad, repeat steps 3–5 with the iPad selected. With a free Apple ID the app stops opening after 7 days; run it from Xcode again to renew it.

Keep the device and the Master Player on the same LAN. Guest Wi-Fi or router client isolation can block Bonjour discovery. If Windows Firewall asks, allow MapForge Player on the private network.

The screen stays awake while the app is connected to a Player.

## Keyboard shortcuts (iPad with a keyboard)

- **Space**: play / pause
- **B**: blackout / restore
- A scene's single-character hotkey (for example `1`) starts that scene.

## Current controls

- Scene, cue, and loop selection
- Play, pause, stop, mute, volume, blackout, restore, and exit loop
- Player PCs status (master and subs)
- Projector identification and projector order

The app targets iOS 17 and has no third-party iOS dependencies. Every push that touches `apps/ios` is compiled on a GitHub Mac (the **iOS controller** workflow), so Swift errors show up even when the change was made on Windows.
