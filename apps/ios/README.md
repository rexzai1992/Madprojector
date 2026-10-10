# MapForge iOS controller

The native iPhone/iPad controller discovers MapForge Players with Bonjour on the local Wi-Fi. It connects to the Master automatically, remembers the selected Player, and offers an in-app Player picker if more than one Master is available. It does not require typing an IP address or port.

## Build and run

Open `MapForgeController/MapForgeController.xcodeproj` in Xcode on a Mac, select the `MapForgeController` scheme, choose an iPhone or iPad, then Run. On first launch, allow MapForge to find devices on the local network. Keep the phone and Master Player on the same LAN; guest Wi-Fi or router client isolation can prevent Bonjour discovery. If Windows Firewall asks, allow MapForge Player on the private network so Bonjour can announce the controller.

The Windows Player advertises `_mapforge._tcp` and serves its existing controller API on the discovered port. Install the updated Player on the Master PC before testing discovery. The web controller remains available.

## Current controls

- Scene, cue, and loop selection
- Play, pause, stop, mute, volume, blackout, restore, and exit loop
- Projector identification and projector order

The app targets iOS 17 and has no third-party iOS dependencies. Building or signing the iOS app requires macOS and Xcode.
