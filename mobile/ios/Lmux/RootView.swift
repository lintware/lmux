import SwiftUI

struct RootView: View {
    @Environment(Store.self) private var store

    var body: some View {
        switch store.phase {
        case .needsLink, .expired:
            ConnectView()
        case .redeeming where store.tree == nil:
            ConnectView()
        default:
            SessionListView()
        }
    }
}

enum LaunchArgs {
    static func value(_ flag: String) -> String? {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }
}

/// Destination for a tab's detail page.
struct TabRoute: Hashable {
    let surface: String
}
