import SwiftUI

// Building blocks shared by the iPhone and iPad screens. Change the look here
// and both layouts follow.

enum Theme {
    static let background = Color(red: 0.035, green: 0.045, blue: 0.07)
    static let card = Color.white.opacity(0.07)
    static let tile = Color.white.opacity(0.08)
    static let live = Color.green
    static let danger = Color(red: 0.92, green: 0.22, blue: 0.24)
}

extension Color {
    init(rgb: [Int]?, fallback: Color) {
        guard let rgb, rgb.count == 3 else { self = fallback; return }
        self = Color(red: Double(rgb[0]) / 255, green: Double(rgb[1]) / 255, blue: Double(rgb[2]) / 255)
    }
}

func timecode(_ seconds: Double) -> String {
    let value = max(0, Int(seconds))
    let (h, m, s) = (value / 3600, value / 60 % 60, value % 60)
    return h > 0 ? String(format: "%d:%02d:%02d", h, m, s) : String(format: "%d:%02d", m, s)
}

/// A single-character hotkey from the show, usable as a hardware-keyboard shortcut.
func shortcutKey(_ hotkey: String?) -> KeyEquivalent? {
    guard let hotkey, hotkey.count == 1, let character = hotkey.lowercased().first else { return nil }
    return KeyEquivalent(character)
}

struct Card<Content: View>: View {
    private let title: String?
    private let content: Content

    init(_ title: String? = nil, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let title { SectionTitle(title) }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding()
        .background(Theme.card, in: RoundedRectangle(cornerRadius: 18))
    }
}

struct SectionTitle: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text.uppercased()).font(.caption.bold()).foregroundStyle(.secondary)
    }
}

struct ConnectionStatus: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(model.connected ? "Connected to Master" : "Searching for Master",
                  systemImage: model.connected ? "wifi" : "wifi.exclamationmark")
                .font(.headline)
                .foregroundStyle(model.connected ? Theme.live : .orange)
            Text(model.status).font(.caption).foregroundStyle(.secondary)
            if !model.controller.settings.note.isEmpty {
                Text(model.controller.settings.note).font(.subheadline)
            }
        }
    }
}

/// Transport pill: PLAYING, PAUSED, LOOP · name…
struct TransportBadge: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        let text = model.state?.loopName.map { "LOOP · \($0)" }
            ?? model.state?.transport.uppercased() ?? "OFFLINE"
        let color: Color = model.state?.blackout == true ? Theme.danger
            : model.state?.isPlaying == true ? Theme.live : .secondary
        Text(model.state?.blackout == true ? "BLACKOUT" : text)
            .font(.caption.bold())
            .padding(.horizontal, 10).padding(.vertical, 5)
            .foregroundStyle(color)
            .background(color.opacity(0.15), in: Capsule())
    }
}

/// Playhead bar that moves smoothly between polls.
struct ProgressRow: View {
    @EnvironmentObject private var model: ControllerModel
    var large = false

    var body: some View {
        if let current = model.controller.currentScene {
            TimelineView(.periodic(from: .now, by: 0.25)) { context in
                let position = model.position(at: context.date)
                let duration = current.durationSeconds
                VStack(spacing: 6) {
                    ProgressView(value: duration > 0 ? min(1, position / duration) : 0)
                        .tint(Color(rgb: model.accent, fallback: .blue))
                        .scaleEffect(x: 1, y: large ? 2 : 1, anchor: .center)
                    HStack {
                        Text("\(timecode(position)) / \(timecode(duration))")
                        Spacer()
                        Text("−\(timecode(max(0, duration - position)))")
                    }
                    .font((large ? Font.body : Font.caption).monospacedDigit())
                    .foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// Play / Pause / Stop. Space toggles play on a hardware keyboard.
struct TransportButtons: View {
    @EnvironmentObject private var model: ControllerModel
    var height: CGFloat = 48

    var body: some View {
        HStack(spacing: 10) {
            button("Play", "play.fill", Color(rgb: model.accent, fallback: .blue), active: model.state?.isPlaying == true) { model.play() }
            button("Pause", "pause.fill", .gray, active: model.state?.transport == "paused") { model.pause() }
            button("Stop", "stop.fill", Theme.danger.opacity(0.85), active: false) { model.stopShow() }
        }
        .background {
            // Invisible keyboard shortcuts for an iPad with a keyboard.
            Button("Play or pause") { model.togglePlay() }
                .keyboardShortcut(.space, modifiers: [])
                .frame(width: 0, height: 0)
                .opacity(0)
                .accessibilityHidden(true)
        }
    }

    private func button(_ label: String, _ icon: String, _ color: Color, active: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(label, systemImage: icon)
                .font(height > 56 ? .title3.bold() : .body.bold())
                .frame(maxWidth: .infinity, minHeight: height)
        }
        .buttonStyle(.borderedProminent)
        .tint(color)
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(.white.opacity(active ? 0.8 : 0), lineWidth: 2))
    }
}

struct BlackoutButton: View {
    @EnvironmentObject private var model: ControllerModel
    var height: CGFloat = 44

    var body: some View {
        let on = model.state?.blackout == true
        Button { model.toggleBlackout() } label: {
            Label(on ? "Restore" : "Blackout", systemImage: on ? "sun.max.fill" : "moon.fill")
                .font(.body.bold())
                .frame(maxWidth: .infinity, minHeight: height)
        }
        .buttonStyle(.borderedProminent)
        .tint(on ? Theme.live : Theme.danger)
        .keyboardShortcut("b", modifiers: [])
    }
}

struct VolumeRow: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        HStack(spacing: 12) {
            Button { model.toggleMute() } label: {
                Image(systemName: model.state?.muted == true ? "speaker.slash.fill" : "speaker.wave.2.fill")
                    .frame(width: 28)
            }
            .tint(model.state?.muted == true ? .orange : .primary)
            Slider(value: Binding(get: { model.volumeDraft }, set: model.setVolume), in: 0...1)
            Text(model.state?.muted == true ? "MUTED" : "\(Int(model.volumeDraft * 100))%")
                .font(.caption.monospacedDigit().bold())
                .frame(width: 52, alignment: .trailing)
        }
    }
}

struct SceneTile: View {
    @EnvironmentObject private var model: ControllerModel
    let scene: ControlScene
    var height: CGFloat = 52

    var body: some View {
        let live = scene.id == model.state?.sceneId
        let tint = Color(rgb: scene.color, fallback: Color(rgb: model.accent, fallback: .blue))
        Button { model.startScene(scene) } label: {
            HStack(spacing: 10) {
                Text(String(format: "%02d", scene.index + 1))
                    .font(.caption.bold().monospacedDigit())
                    .frame(width: 34, height: 34)
                    .background(tint.opacity(0.35), in: Circle())
                VStack(alignment: .leading, spacing: 2) {
                    Text(scene.label).font(height > 60 ? .headline : .subheadline.bold()).lineLimit(2)
                    if live { Text("LIVE").font(.caption2.bold()).foregroundStyle(Theme.live) }
                }
                Spacer(minLength: 0)
                if let hotkey = scene.hotkey, !hotkey.isEmpty {
                    Text(hotkey).font(.caption2.monospaced()).foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, minHeight: height)
            .padding(.horizontal, 10)
            .background(live ? Theme.live.opacity(0.22) : Theme.tile, in: RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 14).stroke(live ? Theme.live : tint.opacity(0.35), lineWidth: live ? 2 : 1))
        }
        .buttonStyle(.plain)
        .modifier(OptionalShortcut(key: shortcutKey(scene.hotkey)))
    }
}

struct OptionalShortcut: ViewModifier {
    let key: KeyEquivalent?
    @ViewBuilder func body(content: Content) -> some View {
        if let key { content.keyboardShortcut(key, modifiers: []) } else { content }
    }
}

/// Cue (jump) and loop buttons for one scene.
struct CueButtons: View {
    @EnvironmentObject private var model: ControllerModel
    let scene: ControlScene
    var minWidth: CGFloat = 140
    var height: CGFloat = 44

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: minWidth), spacing: 8)], spacing: 8) {
            ForEach(scene.cues) { cue in
                pad("◆  \(cue.label)", color: cue.color, active: false) { model.jump(to: cue, in: scene) }
            }
            ForEach(scene.loops) { loop in
                let active = model.state?.sceneId == scene.id && model.state?.loopName == (loop.name ?? loop.label)
                pad("↻  \(loop.label)", color: loop.color, active: active) { model.enter(loop, in: scene) }
            }
        }
    }

    private func pad(_ text: String, color: [Int]?, active: Bool, action: @escaping () -> Void) -> some View {
        let tint = Color(rgb: color, fallback: .white)
        return Button(action: action) {
            Text(text).font(.subheadline.bold()).lineLimit(2)
                .frame(maxWidth: .infinity, minHeight: height)
                .padding(.horizontal, 8)
                .background(active ? Theme.live.opacity(0.25) : tint.opacity(0.12), in: RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(active ? Theme.live : tint.opacity(0.3), lineWidth: 1))
        }
        .buttonStyle(.plain)
    }
}

struct ProjectorOrderList: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                SectionTitle("Projector order")
                Spacer()
                Button(model.projectors.identifying ? "Hide numbers" : "Show numbers") { model.toggleCalibration() }
                    .font(.caption.bold())
            }
            let ordered = model.orderedProjectors
            ForEach(Array(ordered.enumerated()), id: \.element.id) { index, projector in
                HStack(spacing: 12) {
                    Text("\(index + 1)").font(.headline.bold())
                        .frame(width: 34, height: 34)
                        .background(Color(rgb: model.accent, fallback: .blue), in: Circle())
                    VStack(alignment: .leading) {
                        Text(projector.name).font(.subheadline.bold())
                        Text("Output #\(projector.number) · X \(Int(projector.x))  Y \(Int(projector.y))")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button { model.move(projector, by: -1) } label: { Image(systemName: "chevron.up") }
                        .disabled(index == 0)
                    Button { model.move(projector, by: 1) } label: { Image(systemName: "chevron.down") }
                        .disabled(index == ordered.count - 1)
                }
                .padding(10)
                .background(.black.opacity(0.2), in: RoundedRectangle(cornerRadius: 12))
            }
        }
    }
}

/// The master and the sub PCs it drives, with whether each one answers.
struct PlayerPCsList: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            SectionTitle("Player PCs")
            row(model.endpoint?.name ?? "Master", detail: "Master", online: model.connected)
            ForEach(model.state?.followers ?? []) { follower in
                row(follower.address, detail: "Sub", online: follower.online)
            }
        }
    }

    private func row(_ name: String, detail: String, online: Bool) -> some View {
        HStack(spacing: 10) {
            Circle().fill(online ? Theme.live : Theme.danger).frame(width: 9, height: 9)
            Text(name).font(.subheadline).lineLimit(1)
            Spacer()
            Text(online ? detail : "\(detail) · offline").font(.caption).foregroundStyle(online ? Color.secondary : Theme.danger)
        }
    }
}

struct PlayersList: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionTitle("Players on this Wi-Fi")
            if model.availablePlayers.isEmpty {
                HStack(spacing: 10) { ProgressView(); Text("Looking…").foregroundStyle(.secondary) }
            }
            ForEach(model.availablePlayers) { player in
                Button { model.connect(player) } label: {
                    HStack {
                        Image(systemName: "display")
                        Text(player.name)
                        Spacer()
                        if player == model.endpoint { Image(systemName: "checkmark").foregroundStyle(Theme.live) }
                        else { Image(systemName: "chevron.right") }
                    }
                    .padding()
                    .background(Theme.tile, in: RoundedRectangle(cornerRadius: 12))
                }
                .buttonStyle(.plain)
            }
        }
    }
}

struct PlayerPickerSheet: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        NavigationStack {
            ScrollView { PlayersList().padding() }
                .navigationTitle("Choose Player")
                .toolbar {
                    ToolbarItem(placement: .topBarTrailing) { Button("Done") { model.choosePlayer = false } }
                }
        }
        .presentationDetents([.medium, .large])
    }
}
