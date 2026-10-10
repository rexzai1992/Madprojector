import SwiftUI
import Network

@main
struct MapForgeControllerApp: App {
    @StateObject private var model = ControllerModel()

    var body: some Scene {
        WindowGroup { ControllerView().environmentObject(model) }
    }
}

struct PlayerEndpoint: Identifiable, Hashable {
    var id: String { name }
    let name: String
    let host: String
    let port: Int
}

@MainActor
final class LanDiscovery: NSObject, ObservableObject, NetServiceBrowserDelegate, NetServiceDelegate {
    @Published private(set) var players: [PlayerEndpoint] = []
    var onChange: (([PlayerEndpoint]) -> Void)?
    private let browser = NetServiceBrowser()
    private var resolving: [String: NetService] = [:]

    override init() {
        super.init()
        browser.delegate = self
    }

    func start() { browser.searchForServices(ofType: "_mapforge._tcp.", inDomain: "local.") }
    func stop() { browser.stop(); resolving.removeAll(); players.removeAll() }

    func netServiceBrowser(_ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool) {
        resolving[service.name] = service
        service.delegate = self
        service.resolve(withTimeout: 6)
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        guard let rawHost = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
              !rawHost.isEmpty else { return }
        let endpoint = PlayerEndpoint(name: sender.name, host: rawHost, port: sender.port)
        if let index = players.firstIndex(where: { $0.name == endpoint.name }) { players[index] = endpoint }
        else { players.append(endpoint) }
        players.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        onChange?(players)
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didRemove service: NetService, moreComing: Bool) {
        players.removeAll { $0.name == service.name }
        resolving.removeValue(forKey: service.name)
        onChange?(players)
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        resolving.removeValue(forKey: sender.name)
    }
}

struct PlayerState: Decodable {
    var role: String?
    var transport: String = "stopped"
    var sceneId: String?
    var positionSeconds: Double = 0
    var volume: Double = 1
    var muted = false
    var blackout = false
    var loopName: String?
}

struct ControlSettings: Decodable {
    var title = "MapForge Live Control"
    var note = ""
    var accent: [Int] = [36, 107, 254]
    var showTransport = true
    var showScenes = true
    var showBlackout = true
    var showVolume = true
}

struct ControlButton: Decodable, Identifiable {
    var index: Int
    var label: String
    var color: [Int]?
    var hotkey: String?
    var name: String?
    var id: String { "\(index)-\(label)" }
}

struct ControlScene: Decodable, Identifiable {
    var index: Int
    var id: String
    var label: String
    var hotkey: String?
    var cues: [ControlButton] = []
    var loops: [ControlButton] = []
}

struct CurrentScene: Decodable {
    var id: String
    var label: String
    var durationSeconds: Double = 0
}

struct ControllerDocument: Decodable {
    var settings = ControlSettings()
    var scenes: [ControlScene] = []
    var currentScene: CurrentScene?
}

struct Projector: Decodable, Identifiable {
    var id: String
    var name: String
    var number: Int
    var position: Int
    var x: Double
    var y: Double
}

struct ProjectorDocument: Decodable {
    var outputs: [Projector] = []
    var identifying = false
}

@MainActor
final class ControllerModel: ObservableObject {
    @Published var discovery = LanDiscovery()
    @Published private(set) var availablePlayers: [PlayerEndpoint] = []
    @Published var endpoint: PlayerEndpoint?
    @Published var state: PlayerState?
    @Published var controller = ControllerDocument()
    @Published var projectors = ProjectorDocument()
    @Published var error = "Searching for MapForge Players on this Wi-Fi…"
    @Published var busy = false
    @Published var volumeDraft: Double = 1
    @Published var choosePlayer = false
    private var polling: Task<Void, Never>?
    private var lastVolume = Date.distantPast
    private let savedPlayerKey = "mapforge.preferred-player"

    init() {
        discovery.onChange = { [weak self] players in self?.availablePlayers = players }
    }

    func start() {
        discovery.start()
        polling?.cancel()
        polling = Task {
            while !Task.isCancelled {
                await findMasterIfNeeded()
                if endpoint != nil { await refresh() }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    func stop() { polling?.cancel(); discovery.stop() }

    private func findMasterIfNeeded() async {
        guard endpoint == nil, !availablePlayers.isEmpty else { return }
        let preferred = UserDefaults.standard.string(forKey: savedPlayerKey)
        let choices = availablePlayers.sorted { ($0.name == preferred) && ($1.name != preferred) }
        for candidate in choices {
            do {
                let (data, _) = try await URLSession.shared.data(from: candidate.url.appending(path: "api/state"))
                let player = try JSONDecoder().decode(PlayerState.self, from: data)
                if player.role == "master" || preferred == candidate.name {
                    endpoint = candidate
                    UserDefaults.standard.set(candidate.name, forKey: savedPlayerKey)
                    error = "Connected to \(candidate.name)"
                    return
                }
            } catch { continue }
        }
        error = "No Master Player found. Check that the Master Player is open and on the same Wi-Fi."
    }

    func connect(_ player: PlayerEndpoint) {
        endpoint = player
        UserDefaults.standard.set(player.name, forKey: savedPlayerKey)
        choosePlayer = false
        error = "Connecting to \(player.name)…"
        Task { await refresh() }
    }

    func disconnect() {
        endpoint = nil
        state = nil
        UserDefaults.standard.removeObject(forKey: savedPlayerKey)
        error = "Choose a MapForge Player on this Wi-Fi."
        choosePlayer = true
    }

    func refresh() async {
        guard let endpoint else { return }
        do {
            async let stateData = request("api/state", endpoint: endpoint)
            async let controllerData = request("api/controller", endpoint: endpoint)
            async let projectorData = request("api/projectors", endpoint: endpoint)
            let (sd, cd, pd) = try await (stateData, controllerData, projectorData)
            let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
            state = try decoder.decode(PlayerState.self, from: sd)
            controller = try decoder.decode(ControllerDocument.self, from: cd)
            projectors = try decoder.decode(ProjectorDocument.self, from: pd)
            if let volume = state?.volume { volumeDraft = volume }
            error = "Connected · \(endpoint.name)"
        } catch {
            self.error = "Can’t reach \(endpoint.name). Reconnecting…"
        }
    }

    func command(_ path: String) {
        Task { do { _ = try await request("api/\(path)", method: "POST"); await refresh() }
              catch { self.error = error.localizedDescription } }
    }

    func setVolume(_ value: Double) {
        volumeDraft = value
        guard Date().timeIntervalSince(lastVolume) > 0.15 else { return }
        lastVolume = Date()
        command("volume/\(String(format: "%.2f", value))")
    }

    func toggleCalibration() { command("calibration") }

    func move(_ projector: Projector, by delta: Int) {
        var ordered = projectors.outputs.sorted { $0.position < $1.position }
        guard let from = ordered.firstIndex(where: { $0.id == projector.id }) else { return }
        let to = from + delta
        guard ordered.indices.contains(to) else { return }
        ordered.swapAt(from, to)
        guard let endpoint else { return }
        Task {
            do {
                let body = try JSONSerialization.data(withJSONObject: ["order": ordered.map(\.id)])
                _ = try await request("api/projector-order", method: "POST", body: body, endpoint: endpoint)
                await refresh()
            } catch { self.error = error.localizedDescription }
        }
    }

    private func request(_ path: String, method: String = "GET", body: Data? = nil,
                         endpoint selected: PlayerEndpoint? = nil) async throws -> Data {
        guard let target = selected ?? endpoint else { throw URLError(.notConnectedToInternet) }
        var request = URLRequest(url: target.url.appending(path: path))
        request.httpMethod = method
        request.httpBody = body
        if body != nil { request.setValue("application/json", forHTTPHeaderField: "Content-Type") }
        request.timeoutInterval = 4
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let response = response as? HTTPURLResponse, (200..<300).contains(response.statusCode) else {
            throw URLError(.badServerResponse)
        }
        return data
    }
}

extension PlayerEndpoint {
    var url: URL { URL(string: "http://\(host):\(port)/")! }
}

struct ControllerView: View {
    @EnvironmentObject private var model: ControllerModel
    private let blue = Color(red: 0.20, green: 0.43, blue: 0.98)

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 16) {
                    connectionCard
                    if model.endpoint != nil {
                        playbackCard
                        if model.controller.settings.showScenes { scenesCard }
                        if !model.projectors.outputs.isEmpty { projectorCard }
                    } else if !model.availablePlayers.isEmpty { playersCard }
                }
                .padding(16)
            }
            .background(Color(red: 0.035, green: 0.045, blue: 0.07).ignoresSafeArea())
            .navigationTitle(model.controller.settings.title)
            .toolbar { ToolbarItem(placement: .topBarTrailing) {
                Button { model.choosePlayer = true } label: { Image(systemName: "dot.radiowaves.left.and.right") }
            } }
            .preferredColorScheme(.dark)
            .task { model.start() }
            .onDisappear { model.stop() }
            .sheet(isPresented: $model.choosePlayer) { NavigationStack { playersCard.padding().navigationTitle("Choose Player").toolbar { ToolbarItem(placement: .topBarTrailing) { Button("Done") { model.choosePlayer = false } } } }.presentationDetents([.medium, .large]) }
        }
    }

    private var connectionCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(model.state == nil ? "Searching for Master" : "Connected to Master", systemImage: model.state == nil ? "wifi.exclamationmark" : "wifi")
                .font(.headline).foregroundStyle(model.state == nil ? .orange : .green)
            Text(model.error).font(.caption).foregroundStyle(.secondary)
            if !model.controller.settings.note.isEmpty { Text(model.controller.settings.note).font(.subheadline) }
        }.frame(maxWidth: .infinity, alignment: .leading).padding().background(.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 18))
    }

    private var playbackCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack { Text("NOW PLAYING").font(.caption.bold()).foregroundStyle(.secondary); Spacer(); Text(model.state?.loopName.map { "LOOP · \($0)" } ?? (model.state?.transport.uppercased() ?? "OFFLINE")).font(.caption.bold()).foregroundStyle(.green) }
            Text(model.controller.currentScene?.label ?? "No scene selected").font(.title2.bold())
            if let current = model.controller.currentScene {
                ProgressView(value: current.durationSeconds > 0 ? min(1, (model.state?.positionSeconds ?? 0) / current.durationSeconds) : 0, tint: blue)
                HStack { Text("\(time(model.state?.positionSeconds ?? 0)) / \(time(current.durationSeconds))"); Spacer(); Text(model.state?.muted == true ? "MUTED" : "\(Int((model.state?.volume ?? 1) * 100))% VOL") }.font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            HStack(spacing: 8) {
                controlButton("Play", "play.fill", "play", blue)
                controlButton("Pause", "pause.fill", "pause", .gray)
                controlButton("Stop", "stop.fill", "stop", .red.opacity(0.8))
            }
            HStack {
                Button(model.state?.muted == true ? "Unmute" : "Mute") { model.command(model.state?.muted == true ? "unmute" : "mute") }.buttonStyle(.bordered)
                Spacer()
                if model.state?.loopName != nil { Button("Exit loop") { model.command("release") }.buttonStyle(.bordered) }
                Button(model.state?.blackout == true ? "Restore" : "Blackout") { model.command(model.state?.blackout == true ? "restore" : "blackout") }.buttonStyle(.bordered).tint(model.state?.blackout == true ? .green : .red)
            }
            if model.controller.settings.showVolume {
                HStack { Image(systemName: "speaker.wave.2"); Slider(value: Binding(get: { model.volumeDraft }, set: model.setVolume), in: 0...1); Text("\(Int(model.volumeDraft * 100))%") }.font(.caption)
            }
        }.padding().background(.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 18))
    }

    private var scenesCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("SCENES").font(.caption.bold()).foregroundStyle(.secondary)
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 140), spacing: 9)], spacing: 9) {
                ForEach(model.controller.scenes) { scene in
                    Button { model.command("scene/\(scene.index)") } label: {
                        HStack { Text(String(format: "%02d", scene.index + 1)).font(.caption.bold()).padding(8).background(.white.opacity(0.12), in: Circle()); Text(scene.label).font(.subheadline.bold()).lineLimit(2); Spacer(minLength: 0) }
                            .frame(maxWidth: .infinity, minHeight: 52).padding(.horizontal, 8)
                            .background(scene.id == model.state?.sceneId ? Color.green.opacity(0.24) : .white.opacity(0.08), in: RoundedRectangle(cornerRadius: 14))
                            .overlay(RoundedRectangle(cornerRadius: 14).stroke(scene.id == model.state?.sceneId ? .green : .white.opacity(0.08), lineWidth: 1))
                    }.buttonStyle(.plain)
                }
            }
            ForEach(model.controller.scenes) { scene in
                if !scene.cues.isEmpty || !scene.loops.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(scene.label).font(.caption.bold()).foregroundStyle(.secondary)
                        ForEach(scene.cues) { cue in Button("◆  \(cue.label)") { model.command("cue/\(scene.index)/\(cue.index)") }.buttonStyle(.bordered) }
                        ForEach(scene.loops) { loop in Button("↻  \(loop.label)") { model.command("loop/\(scene.index)/\(loop.index)") }.buttonStyle(.bordered) }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }.frame(maxWidth: .infinity, alignment: .leading).padding().background(.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 18))
    }

    private var projectorCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack { Text("PROJECTOR ORDER").font(.caption.bold()).foregroundStyle(.secondary); Spacer(); Button(model.projectors.identifying ? "Hide numbers" : "Show numbers") { model.toggleCalibration() }.font(.caption.bold()) }
            ForEach(Array(model.projectors.outputs.sorted { $0.position < $1.position }.enumerated()), id: \.element.id) { index, projector in
                HStack(spacing: 12) {
                    Text("\(index + 1)").font(.headline.bold()).frame(width: 34, height: 34).background(blue, in: Circle())
                    VStack(alignment: .leading) { Text(projector.name).font(.subheadline.bold()); Text("Output #\(projector.number) · X \(Int(projector.x))  Y \(Int(projector.y))").font(.caption).foregroundStyle(.secondary) }
                    Spacer()
                    Button { model.move(projector, by: -1) } label: { Image(systemName: "chevron.up") }.disabled(index == 0)
                    Button { model.move(projector, by: 1) } label: { Image(systemName: "chevron.down") }.disabled(index == model.projectors.outputs.count - 1)
                }.padding(10).background(.black.opacity(0.2), in: RoundedRectangle(cornerRadius: 12))
            }
        }.padding().background(.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 18))
    }

    private var playersCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("PLAYERS ON THIS WI-FI").font(.caption.bold()).foregroundStyle(.secondary)
            ForEach(model.availablePlayers) { player in
                Button { model.connect(player) } label: { HStack { Image(systemName: "display"); Text(player.name); Spacer(); Image(systemName: "chevron.right") }.padding().background(.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 12)) }.buttonStyle(.plain)
            }
        }.frame(maxWidth: .infinity, alignment: .leading).padding().background(.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 18))
    }

    private func controlButton(_ label: String, _ icon: String, _ action: String, _ color: Color) -> some View {
        Button { model.command(action) } label: { Label(label, systemImage: icon).frame(maxWidth: .infinity, minHeight: 48) }.buttonStyle(.borderedProminent).tint(color)
    }

    private func time(_ seconds: Double) -> String {
        let value = max(0, Int(seconds)); return "\(value / 60):\(String(format: "%02d", value % 60))"
    }
}
