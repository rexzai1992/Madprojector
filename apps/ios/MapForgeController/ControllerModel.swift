import Foundation
import Network
import UIKit

// Everything that talks to the MapForge Player lives here. The screens
// (PhoneControllerView, PadControllerView) only read this model and call its
// actions, so the UI can be redesigned without touching the networking.

struct PlayerEndpoint: Identifiable, Hashable {
    var id: String { name }
    let name: String
    let host: String
    let port: Int

    var url: URL { URL(string: "http://\(host):\(port)/") ?? URL(string: "http://127.0.0.1/")! }
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

    // NetService calls back on the main run loop, where it was scheduled.
    nonisolated func netServiceBrowser(_ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool) {
        MainActor.assumeIsolated {
            resolving[service.name] = service
            service.delegate = self
            service.resolve(withTimeout: 6)
        }
    }

    nonisolated func netServiceDidResolveAddress(_ sender: NetService) {
        MainActor.assumeIsolated {
            guard let rawHost = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
                  !rawHost.isEmpty else { return }
            let endpoint = PlayerEndpoint(name: sender.name, host: rawHost, port: sender.port)
            if let index = players.firstIndex(where: { $0.name == endpoint.name }) { players[index] = endpoint }
            else { players.append(endpoint) }
            players.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
            onChange?(players)
        }
    }

    nonisolated func netServiceBrowser(_ browser: NetServiceBrowser, didRemove service: NetService, moreComing: Bool) {
        MainActor.assumeIsolated {
            players.removeAll { $0.name == service.name }
            resolving.removeValue(forKey: service.name)
            onChange?(players)
        }
    }

    nonisolated func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        MainActor.assumeIsolated { _ = resolving.removeValue(forKey: sender.name) }
    }
}

// MARK: - Documents served by the Player
//
// Every field falls back to a default when the Player leaves it out, so a
// newer or older Player never makes the whole screen fail to load.

extension KeyedDecodingContainer {
    func value<T: Decodable>(_ key: Key, or fallback: T) -> T {
        (try? decodeIfPresent(T.self, forKey: key)) ?? fallback
    }

    func optional<T: Decodable>(_ key: Key) -> T? {
        try? decodeIfPresent(T.self, forKey: key)
    }
}

struct Follower: Decodable, Identifiable {
    var address = ""
    var online = false
    var id: String { address }

    enum CodingKeys: String, CodingKey { case address, online }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        address = c.value(.address, or: "")
        online = c.value(.online, or: false)
    }
}

struct PlayerState: Decodable {
    var role: String?
    var transport = "stopped"
    var sceneId: String?
    var positionSeconds: Double = 0
    var volume: Double = 1
    var muted = false
    var blackout = false
    var loopName: String?
    var followers: [Follower] = []

    var isPlaying: Bool { transport == "playing" }

    enum CodingKeys: String, CodingKey {
        case role, transport, sceneId, positionSeconds, volume, muted, blackout, loopName, followers
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        role = c.optional(.role)
        transport = c.value(.transport, or: "stopped")
        sceneId = c.optional(.sceneId)
        positionSeconds = c.value(.positionSeconds, or: 0)
        volume = c.value(.volume, or: 1)
        muted = c.value(.muted, or: false)
        blackout = c.value(.blackout, or: false)
        loopName = c.optional(.loopName)
        followers = c.value(.followers, or: [])
    }
}

struct ControlSettings: Decodable {
    var title = "MapForge Live Control"
    var note = ""
    var accent: [Int] = [36, 107, 254]
    var columns = 2
    var showTransport = true
    var showScenes = true
    var showBlackout = true
    var showVolume = true

    enum CodingKeys: String, CodingKey {
        case title, note, accent, columns, showTransport, showScenes, showBlackout, showVolume
    }

    init() {}
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        title = c.value(.title, or: title)
        note = c.value(.note, or: note)
        accent = c.value(.accent, or: accent)
        columns = c.value(.columns, or: columns)
        showTransport = c.value(.showTransport, or: true)
        showScenes = c.value(.showScenes, or: true)
        showBlackout = c.value(.showBlackout, or: true)
        showVolume = c.value(.showVolume, or: true)
    }
}

struct ControlButton: Decodable, Identifiable {
    var index = 0
    var label = ""
    var color: [Int]?
    var hotkey: String?
    var name: String?
    var id: String { "\(index)-\(label)" }

    enum CodingKeys: String, CodingKey { case index, label, color, hotkey, name }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = c.value(.index, or: 0)
        label = c.value(.label, or: "")
        color = c.optional(.color)
        hotkey = c.optional(.hotkey)
        name = c.optional(.name)
    }
}

struct ControlScene: Decodable, Identifiable {
    var index = 0
    var id = ""
    var label = ""
    var color: [Int]?
    var hotkey: String?
    var cues: [ControlButton] = []
    var loops: [ControlButton] = []

    enum CodingKeys: String, CodingKey { case index, id, label, color, hotkey, cues, loops }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = c.value(.index, or: 0)
        id = c.value(.id, or: "")
        label = c.value(.label, or: "")
        color = c.optional(.color)
        hotkey = c.optional(.hotkey)
        cues = c.value(.cues, or: [])
        loops = c.value(.loops, or: [])
    }
}

struct CurrentScene: Decodable {
    var id = ""
    var label = ""
    var durationSeconds: Double = 0

    enum CodingKeys: String, CodingKey { case id, label, durationSeconds }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = c.value(.id, or: "")
        label = c.value(.label, or: "")
        durationSeconds = c.value(.durationSeconds, or: 0)
    }
}

struct ControllerDocument: Decodable {
    var settings = ControlSettings()
    var scenes: [ControlScene] = []
    var currentScene: CurrentScene?

    enum CodingKeys: String, CodingKey { case settings, scenes, currentScene }
    init() {}
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        settings = c.value(.settings, or: ControlSettings())
        scenes = c.value(.scenes, or: [])
        currentScene = c.optional(.currentScene)
    }
}

struct Projector: Decodable, Identifiable {
    var id = ""
    var name = ""
    var number = 0
    var position = 0
    var x: Double = 0
    var y: Double = 0

    enum CodingKeys: String, CodingKey { case id, name, number, position, x, y }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = c.value(.id, or: "")
        name = c.value(.name, or: "")
        number = c.value(.number, or: 0)
        position = c.value(.position, or: 0)
        x = c.value(.x, or: 0)
        y = c.value(.y, or: 0)
    }
}

struct ProjectorDocument: Decodable {
    var outputs: [Projector] = []
    var identifying = false

    enum CodingKeys: String, CodingKey { case outputs, identifying }
    init() {}
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        outputs = c.value(.outputs, or: [])
        identifying = c.value(.identifying, or: false)
    }
}

// MARK: - Model

@MainActor
final class ControllerModel: ObservableObject {
    @Published var discovery = LanDiscovery()
    @Published private(set) var availablePlayers: [PlayerEndpoint] = []
    @Published var endpoint: PlayerEndpoint?
    @Published var state: PlayerState?
    @Published var controller = ControllerDocument()
    @Published var projectors = ProjectorDocument()
    @Published var status = "Searching for MapForge Players on this Wi-Fi…"
    @Published var choosePlayer = false
    @Published var volumeDraft: Double = 1
    /// When `state` arrived, so the playhead can move smoothly between polls.
    @Published private(set) var stateReceived = Date()

    private var polling: Task<Void, Never>?
    private var lastVolumeSent = Date.distantPast
    private var lastVolumeTouched = Date.distantPast
    private var trailingVolume: Task<Void, Never>?
    /// Polls in a row that failed; after a few the Master is searched again.
    private var failedPolls = 0
    /// Set when the user picked this Player by hand, so it is kept even if
    /// it is not the master.
    private var chosenByHand = false
    private let savedPlayerKey = "mapforge.preferred-player"
    private let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()

    init() {
        discovery.onChange = { [weak self] players in self?.availablePlayers = players }
    }

    var connected: Bool { endpoint != nil && state != nil }
    var accent: [Int] { controller.settings.accent }

    var orderedProjectors: [Projector] { projectors.outputs.sorted { $0.position < $1.position } }

    var currentSceneDetails: ControlScene? {
        guard let id = state?.sceneId else { return nil }
        return controller.scenes.first { $0.id == id }
    }

    /// The show position now, moved on from the last poll while playing.
    func position(at date: Date) -> Double {
        guard let state else { return 0 }
        var seconds = state.positionSeconds
        if state.isPlaying { seconds += max(0, date.timeIntervalSince(stateReceived)) }
        if let duration = controller.currentScene?.durationSeconds, duration > 0, state.loopName == nil {
            seconds = min(seconds, duration)
        }
        return seconds
    }

    /// Starts discovery and polling; calling it again while running does nothing.
    func start() {
        guard polling == nil else { return }
        discovery.start()
        polling = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.findMasterIfNeeded()
                if self.endpoint != nil { await self.refresh() }
                try? await Task.sleep(for: .milliseconds(700))
            }
        }
    }

    func stop() {
        polling?.cancel()
        polling = nil
        discovery.stop()
    }

    private func findMasterIfNeeded() async {
        guard endpoint == nil, !availablePlayers.isEmpty else { return }
        let preferred = UserDefaults.standard.string(forKey: savedPlayerKey)
        let choices = availablePlayers.sorted { ($0.name == preferred) && ($1.name != preferred) }
        // A master always wins (the remembered one first). The remembered
        // Player is used as it is only when no PC on the Wi-Fi is master.
        var rememberedReachable: PlayerEndpoint?
        for candidate in choices {
            guard let data = try? await request("api/state", endpoint: candidate),
                  let player = try? decoder.decode(PlayerState.self, from: data) else { continue }
            guard endpoint == nil else { return }
            if player.role == "master" {
                use(candidate, byHand: false)
                return
            }
            if candidate.name == preferred { rememberedReachable = candidate }
        }
        if let remembered = rememberedReachable, endpoint == nil {
            use(remembered, byHand: true)
            return
        }
        status = "No Master Player found. Check that the Master Player is open and on the same Wi-Fi."
    }

    private func use(_ candidate: PlayerEndpoint, byHand: Bool) {
        endpoint = candidate
        chosenByHand = byHand
        failedPolls = 0
        UserDefaults.standard.set(candidate.name, forKey: savedPlayerKey)
        status = "Connected to \(candidate.name)"
    }

    func connect(_ player: PlayerEndpoint) {
        endpoint = player
        state = nil
        chosenByHand = true
        failedPolls = 0
        UserDefaults.standard.set(player.name, forKey: savedPlayerKey)
        choosePlayer = false
        status = "Connecting to \(player.name)…"
        Task { await refresh() }
    }

    func disconnect() {
        endpoint = nil
        state = nil
        UserDefaults.standard.removeObject(forKey: savedPlayerKey)
        status = "Choose a MapForge Player on this Wi-Fi."
        choosePlayer = true
    }

    func refresh() async {
        guard let endpoint else { return }
        do {
            async let stateData = request("api/state", endpoint: endpoint)
            async let controllerData = request("api/controller", endpoint: endpoint)
            async let projectorData = request("api/projectors", endpoint: endpoint)
            let (sd, cd, pd) = try await (stateData, controllerData, projectorData)
            // The user may have picked another Player while this was in flight.
            guard endpoint == self.endpoint else { return }
            let newState = try decoder.decode(PlayerState.self, from: sd)
            controller = try decoder.decode(ControllerDocument.self, from: cd)
            projectors = try decoder.decode(ProjectorDocument.self, from: pd)
            state = newState
            stateReceived = Date()
            failedPolls = 0
            // The PC was switched to sub: find whichever PC is master now.
            if newState.role == "sub" && !chosenByHand {
                lose(endpoint, reason: "\(endpoint.name) is no longer the Master. Searching…")
                return
            }
            // Don't yank the slider from under a finger that is still moving it.
            if Date().timeIntervalSince(lastVolumeTouched) > 1.5 { volumeDraft = newState.volume }
            status = "Connected · \(endpoint.name)"
        } catch {
            guard endpoint == self.endpoint else { return }
            state = nil
            failedPolls += 1
            if failedPolls >= 3 {
                // Search again; the remembered Player is still preferred when it returns.
                lose(endpoint, reason: "Lost \(endpoint.name). Searching for the Master…")
            } else {
                status = "Can’t reach \(endpoint.name). Reconnecting…"
            }
        }
    }

    private func lose(_ old: PlayerEndpoint, reason: String) {
        guard endpoint == old else { return }
        endpoint = nil
        state = nil
        failedPolls = 0
        chosenByHand = false
        status = reason
    }

    // MARK: Actions

    func command(_ path: String) {
        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        Task {
            do {
                _ = try await request("api/\(path)", method: "POST")
                await refresh()
            } catch {
                status = "Command failed: \(error.localizedDescription)"
            }
        }
    }

    func play() { command("play") }
    func pause() { command("pause") }
    func stopShow() { command("stop") }
    func togglePlay() { state?.isPlaying == true ? pause() : play() }
    func toggleMute() { command(state?.muted == true ? "unmute" : "mute") }
    func toggleBlackout() { command(state?.blackout == true ? "restore" : "blackout") }
    func releaseLoop() { command("release") }
    func startScene(_ scene: ControlScene) { command("scene/\(scene.index)") }
    func jump(to cue: ControlButton, in scene: ControlScene) { command("cue/\(scene.index)/\(cue.index)") }
    func enter(_ loop: ControlButton, in scene: ControlScene) { command("loop/\(scene.index)/\(loop.index)") }
    func toggleCalibration() { command("calibration") }

    /// Sends at most one volume every 0.15 s while the slider moves, and
    /// always sends the value the finger stopped on, so a quick fade to 0
    /// really ends at 0.
    func setVolume(_ value: Double) {
        volumeDraft = value
        lastVolumeTouched = Date()
        let wait = 0.15 - Date().timeIntervalSince(lastVolumeSent)
        if wait <= 0 {
            sendVolume()
        } else if trailingVolume == nil {
            trailingVolume = Task { [weak self] in
                try? await Task.sleep(for: .seconds(wait))
                guard let self, !Task.isCancelled else { return }
                self.trailingVolume = nil
                self.sendVolume()
            }
        }
    }

    private func sendVolume() {
        lastVolumeSent = Date()
        let value = volumeDraft
        Task { _ = try? await request("api/volume/\(String(format: "%.2f", value))", method: "POST") }
    }

    func move(_ projector: Projector, by delta: Int) {
        var ordered = orderedProjectors
        guard let from = ordered.firstIndex(where: { $0.id == projector.id }) else { return }
        let to = from + delta
        guard ordered.indices.contains(to), let endpoint else { return }
        ordered.swapAt(from, to)
        Task {
            do {
                let body = try JSONSerialization.data(withJSONObject: ["order": ordered.map(\.id)])
                _ = try await request("api/projector-order", method: "POST", body: body, endpoint: endpoint)
                await refresh()
            } catch { status = error.localizedDescription }
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
