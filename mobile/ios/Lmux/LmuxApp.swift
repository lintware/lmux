import SwiftUI

@main
struct LmuxApp: App {
    @State private var store = Store()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(store)
                .tint(Theme.accent)
                // `lmux://share?url=…` or `lmux://<host>/#<code>`, and https share
                // links handed to the app.
                .onOpenURL { url in
                    Task { _ = await store.open(url: url) }
                }
                .task {
                    // Verification hook, since a simulator cannot scan or tap:
                    // `simctl launch <udid> com.vas.lmux -link <share link> [-tab surface:N]`.
                    if let link = LaunchArgs.value("-link"), let url = URL(string: link) {
                        _ = await store.open(url: url)
                    }
                }
        }
        .onChange(of: scenePhase) { _, phase in
            store.setForeground(phase == .active)
        }
    }
}
