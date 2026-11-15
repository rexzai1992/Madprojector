import SwiftUI

/// iPhone layout: one scrolling column. Uses the same model and building
/// blocks as the iPad layout; only the arrangement differs.
struct PhoneControllerView: View {
    @EnvironmentObject private var model: ControllerModel

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 16) {
                    Card { ConnectionStatus() }
                    if model.endpoint != nil {
                        nowPlaying
                        if model.controller.settings.showScenes { scenes }
                        if !model.projectors.outputs.isEmpty { Card { ProjectorOrderList() } }
                        if !(model.state?.followers ?? []).isEmpty { Card { PlayerPCsList() } }
                    } else {
                        Card { PlayersList() }
                    }
                }
                .padding(16)
            }
            .background(Theme.background.ignoresSafeArea())
            .navigationTitle(model.controller.settings.title)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button { model.choosePlayer = true } label: { Image(systemName: "dot.radiowaves.left.and.right") }
                }
            }
        }
    }

    private var nowPlaying: some View {
        Card {
            HStack {
                SectionTitle("Now playing")
                Spacer()
                TransportBadge()
            }
            Text(model.controller.currentScene?.label ?? "No scene selected").font(.title2.bold())
            ProgressRow()
            if model.controller.settings.showTransport { TransportButtons() }
            HStack {
                if model.state?.loopName != nil {
                    Button("Exit loop") { model.releaseLoop() }.buttonStyle(.bordered)
                }
                if model.controller.settings.showBlackout { BlackoutButton(height: 36) }
            }
            if model.controller.settings.showVolume { VolumeRow() }
        }
    }

    private var scenes: some View {
        Card("Scenes") {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 140), spacing: 9)], spacing: 9) {
                ForEach(model.controller.scenes) { SceneTile(scene: $0) }
            }
            ForEach(model.controller.scenes) { scene in
                if !scene.cues.isEmpty || !scene.loops.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(scene.label).font(.caption.bold()).foregroundStyle(.secondary)
                        CueButtons(scene: scene)
                    }
                }
            }
        }
    }
}
