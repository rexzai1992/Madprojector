import SwiftUI
import UIKit

@main
struct MapForgeControllerApp: App {
    @StateObject private var model = ControllerModel()

    var body: some Scene {
        WindowGroup { RootView().environmentObject(model) }
    }
}

/// Picks the layout for the screen: the iPad desk when there is room (iPad
/// full screen or a wide split), the single column on iPhone. Both share
/// ControllerModel, so a UI change never touches the networking.
struct RootView: View {
    @EnvironmentObject private var model: ControllerModel
    @Environment(\.horizontalSizeClass) private var sizeClass
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        Group {
            if sizeClass == .regular { PadControllerView() } else { PhoneControllerView() }
        }
        .preferredColorScheme(.dark)
        .sheet(isPresented: $model.choosePlayer) { PlayerPickerSheet() }
        .task { model.start() }
        .onChange(of: scenePhase) { _, phase in
            // Stop polling in the background; pick up again on return.
            if phase == .active { model.start() } else if phase == .background { model.stop() }
        }
        .onChange(of: model.endpoint) { _, endpoint in
            // A controller that dims and locks mid-show is no use.
            UIApplication.shared.isIdleTimerDisabled = endpoint != nil
        }
    }
}
