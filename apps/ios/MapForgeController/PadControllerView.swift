import SwiftUI

/// iPad layout: a show-control desk. A sidebar holds connection, Player PCs
/// and projector order; the main area keeps the live controls large and the
/// scene and cue buttons within thumb reach.
struct PadControllerView: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        HStack(spacing: 0) {
            sidebar
                .frame(width: 330)
                .background(Color.black.opacity(0.25))
            Divider()
            if model.endpoint == nil {
                searching
            } else {
                desk
            }
        }
        .background(Theme.background.ignoresSafeArea())
    }

    // MARK: Sidebar

    private var sidebar: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                HStack {
                    Text(model.controller.settings.title).font(.title2.bold()).lineLimit(2)
                    Spacer()
                    Button { model.choosePlayer = true } label: {
                        Image(systemName: "dot.radiowaves.left.and.right").font(.title3)
                    }
                    .accessibilityLabel("Choose Player")
                }
                Card { ConnectionStatus() }
                if model.endpoint != nil {
                    Card { PlayerPCsList() }
                    if !model.projectors.outputs.isEmpty { Card { ProjectorOrderList() } }
                } else {
                    Card { PlayersList() }
                }
            }
            .padding(20)
        }
    }

    private var searching: some View {
        VStack(spacing: 16) {
            ProgressView().controlSize(.large)
            Text("Looking for the Master Player…").font(.title2.bold())
            Text("Open MapForge Player on the Master PC and keep this iPad on the same Wi-Fi.")
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: Desk

    private var desk: some View {
        ScrollView {
            VStack(spacing: 18) {
                nowPlaying
                if model.controller.settings.showScenes { scenes }
                if let scene = model.currentSceneDetails, !scene.cues.isEmpty || !scene.loops.isEmpty {
                    Card("Cues & loops · \(scene.label)") {
                        CueButtons(scene: scene, minWidth: 180, height: 64)
                    }
                }
            }
            .padding(24)
        }
    }

    private var nowPlaying: some View {
        Card {
            HStack(alignment: .firstTextBaseline) {
                SectionTitle("Now playing")
                Spacer()
                TransportBadge()
            }
            Text(model.controller.currentScene?.label ?? "No scene selected")
                .font(.system(size: 40, weight: .bold))
                .lineLimit(1)
                .minimumScaleFactor(0.5)
            ProgressRow(large: true)
            // Side by side in landscape; stacked when the iPad is upright.
            ViewThatFits(in: .horizontal) {
                HStack(alignment: .top, spacing: 14) { liveButtons(blackoutWidth: 190) }
                VStack(spacing: 12) { liveButtons(blackoutWidth: nil) }
            }
            HStack(spacing: 16) {
                if model.controller.settings.showVolume { VolumeRow() }
                if model.state?.loopName != nil {
                    Button { model.releaseLoop() } label: {
                        Label("Exit loop", systemImage: "arrow.uturn.right").frame(minHeight: 36)
                    }
                    .buttonStyle(.bordered)
                    .tint(.orange)
                }
            }
        }
    }

    @ViewBuilder
    private func liveButtons(blackoutWidth: CGFloat?) -> some View {
        if model.controller.settings.showTransport {
            // Side by side only when Play, Pause and Stop still get room.
            TransportButtons(height: 76).frame(minWidth: blackoutWidth == nil ? nil : 420)
        }
        if model.controller.settings.showBlackout {
            BlackoutButton(height: 76).frame(width: blackoutWidth)
        }
    }

    private var scenes: some View {
        Card("Scenes") {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 210), spacing: 12)], spacing: 12) {
                ForEach(model.controller.scenes) { SceneTile(scene: $0, height: 84) }
            }
        }
    }
}
