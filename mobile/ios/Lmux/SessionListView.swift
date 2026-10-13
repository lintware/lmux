import SwiftUI

/// The sessions screen, laid out like an inbox: the connection at the top (tap
/// to switch), a server filter, what needs the user across every server, then
/// each server's sessions. Every row is one session: its agent, what it is
/// doing, and its tabs; a waiting one can be answered from the list.
struct SessionListView: View {
    @Environment(Store.self) private var store
    @State private var path = NavigationPath()
    @State private var showConnections = false
    /// The server the list is narrowed to; nil shows all.
    @State private var filter: String?

    var body: some View {
        NavigationStack(path: $path) {
            content
                .background(Theme.pageBackground.ignoresSafeArea())
                .navigationTitle(store.activeConnection?.displayName ?? "lmux")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .principal) { connectionButton }
                }
                .toolbarBackground(Theme.pageBackground, for: .navigationBar)
                .toolbarBackground(.visible, for: .navigationBar)
                .navigationDestination(for: TabRoute.self) { SessionDetailView(surface: $0.surface) }
        }
        .sheet(isPresented: $showConnections) { ConnectionsView() }
        .onChange(of: store.tree != nil) { _, ready in
            if ready { openLaunchTab() }
        }
        .task {
            // Mock data is there from the start, so `onChange` never fires; wait
            // for the stack to register its destinations first.
            try? await Task.sleep(nanoseconds: 300_000_000)
            if store.tree != nil { openLaunchTab() }
            if CommandLine.arguments.contains("-connections") { showConnections = true }
        }
        .onChange(of: store.serverGroups.map(\.name)) { _, names in
            if let f = filter, !names.contains(f) { filter = nil }
        }
        .onChange(of: store.activeID) { _, _ in
            path = NavigationPath()
            filter = nil
        }
    }

    /// Verification hook: `-tab surface:N` opens that tab once the list loads.
    private func openLaunchTab() {
        if path.isEmpty, let surface = LaunchArgs.value("-tab") {
            path.append(TabRoute(surface: surface))
        }
    }

    private func open(_ surface: String) {
        path.append(TabRoute(surface: surface))
    }

    // MARK: - Content

    private var groups: [ServerGroup] {
        let all = store.serverGroups
        guard let filter else { return all }
        return all.filter { $0.name == filter }
    }

    private var needsYou: [Workspace] { groups.flatMap(\.needsInput) }

    @ViewBuilder
    private var content: some View {
        if store.tree == nil {
            VStack(spacing: 12) {
                ProgressView().tint(Theme.accent)
                Text(statusLine).font(.footnote).foregroundStyle(Theme.secondaryText)
                Button("Connections") { showConnections = true }
                    .font(.footnote.weight(.semibold))
                    .padding(.top, 8)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List {
                banner
                if !needsYou.isEmpty {
                    Section {
                        ForEach(needsYou) { ws in
                            SessionRow(workspace: ws, showServer: store.isMultiServer && filter == nil, open: open)
                        }
                    } header: {
                        SectionHeader(title: "Needs you", dot: Theme.needsInput, count: needsYou.count)
                    }
                }
                ForEach(groups) { group in
                    let rest = group.workspaces.filter { AgentState($0.agent) != .needsInput }
                    if !rest.isEmpty || !group.isConnected || group.workspaces.isEmpty {
                        Section {
                            serverRows(group, rest)
                        } header: {
                            SectionHeader(title: group.name, dot: serverColor(group),
                                          count: group.workspaces.count,
                                          status: group.isConnected ? nil : group.status)
                        }
                    }
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .environment(\.defaultMinListRowHeight, 0)
            .safeAreaInset(edge: .top, spacing: 0) {
                if store.isMultiServer { serverBar }
            }
            .animation(.snappy(duration: 0.22), value: filter)
        }
    }

    @ViewBuilder
    private func serverRows(_ group: ServerGroup, _ rest: [Workspace]) -> some View {
        if group.workspaces.isEmpty || !group.isConnected {
            Label(group.isConnected ? "No sessions" : (group.info?.error ?? "Not reachable"),
                  systemImage: group.isConnected ? "tray" : "exclamationmark.triangle")
                .font(.footnote)
                .foregroundStyle(group.isConnected ? Theme.tertiaryText : Theme.danger)
                .lineLimit(2)
                .listRowBackground(Theme.pageBackground)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 8, leading: 20, bottom: 12, trailing: 20))
        }
        ForEach(rest) { ws in
            SessionRow(workspace: ws, showServer: false, open: open)
        }
    }

    // MARK: - Server bar

    private var serverBar: some View {
        let all = store.serverGroups
        return ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ServerChip(name: "All", dot: nil,
                           count: all.reduce(0) { $0 + $1.needsInput.count }, selected: filter == nil) {
                    filter = nil
                }
                ForEach(all) { group in
                    ServerChip(name: group.name, dot: serverColor(group), count: group.needsInput.count,
                               selected: filter == group.name) {
                        filter = filter == group.name ? nil : group.name
                    }
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
        }
        .background(Theme.pageBackground)
    }

    // MARK: - Chrome

    @ViewBuilder
    private var banner: some View {
        if case let .reconnecting(message) = store.phase {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "arrow.triangle.2.circlepath")
                    .foregroundStyle(Theme.needsInput)
                VStack(alignment: .leading, spacing: 4) {
                    Text("\(message) Reconnecting…")
                        .font(.footnote)
                        .foregroundStyle(Theme.softText)
                    // A stopped share comes back under a new address, so only a new QR helps.
                    Button("Switch or scan a new QR code") { showConnections = true }
                        .font(.footnote.weight(.semibold))
                        .buttonStyle(.borderless)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.needsInputBg, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .listRowBackground(Theme.pageBackground)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 4, trailing: 16))
        }
    }

    private var statusLine: String {
        switch store.phase {
        case let .reconnecting(m): return "\(m) Reconnecting…"
        default: return "Connecting to \(store.activeConnection?.displayName ?? "the share")…"
        }
    }

    /// The connection's name with its link state; opens the connections sheet.
    private var connectionButton: some View {
        Button { showConnections = true } label: {
            VStack(spacing: 1) {
                HStack(spacing: 4) {
                    Text(store.activeConnection?.displayName ?? "lmux")
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(Theme.primaryText)
                        .lineLimit(1)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .bold))
                        .foregroundStyle(Theme.tertiaryText)
                }
                HStack(spacing: 5) {
                    Circle().fill(store.isLive ? Theme.running : Theme.needsInput).frame(width: 6, height: 6)
                    Text(connectionDetail).lineLimit(1)
                }
                .font(.caption2)
                .foregroundStyle(Theme.secondaryText)
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Connection \(store.activeConnection?.displayName ?? ""), \(store.isLive ? "connected" : "offline")")
        .accessibilityHint("Switch connections")
    }

    private var connectionDetail: String {
        guard store.isLive, let tree = store.tree else { return store.isLive ? "Connected" : "Offline" }
        var parts: [String] = []
        if store.isMultiServer { parts.append("\(store.serverGroups.count) servers") }
        let count = tree.workspaces.count
        parts.append(count == 1 ? "1 session" : "\(count) sessions")
        if !tree.typing { parts.append("view only") }
        return parts.joined(separator: " · ")
    }
}

func serverColor(_ group: ServerGroup) -> Color {
    switch group.status {
    case "connected": return Theme.running
    case "connecting": return Theme.needsInput
    default: return Theme.danger
    }
}

// MARK: - Rows

/// One session: its agent's avatar with a status badge, the name and state, a
/// preview of what the agent said, branch and folder, and its tabs. Tapping
/// opens the agent's tab; a waiting session shows its choices as buttons.
struct SessionRow: View {
    let workspace: Workspace
    let showServer: Bool
    let open: (String) -> Void
    @Environment(Store.self) private var store
    @State private var answered: String?

    private var state: AgentState { AgentState(workspace.agent) }
    private var primary: Tab? { workspace.primaryTab }
    private var prompt: PromptOptions? {
        state == .needsInput ? PromptOptions.fromSummary(workspace.cleanSummary) : nil
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            AgentAvatar(symbol: primary?.symbol ?? "terminal", size: 40, state: state, tinted: state == .needsInput)
            VStack(alignment: .leading, spacing: 4) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(workspace.displayTitle)
                        .font(.body.weight(.semibold))
                        .foregroundStyle(Theme.primaryText)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    if let label = statusLabel {
                        Text(label)
                            .font(.caption.weight(state == .needsInput ? .semibold : .regular))
                            .foregroundStyle(state == .idle ? Theme.tertiaryText : state.foreground)
                    }
                }
                if let preview {
                    Text(preview)
                        .font(.subheadline)
                        .foregroundStyle(state == .needsInput ? Theme.softText : Theme.secondaryText)
                        .lineLimit(state == .needsInput ? 3 : 2)
                }
                meta
                if workspace.openableTabs.count > 1 { tabChips }
                if let prompt, store.typingAllowed, let tab = primary { answers(prompt, tab) }
            }
        }
        .padding(.vertical, 12)
        .contentShape(Rectangle())
        .onTapGesture {
            if let tab = primary { open(tab.id) }
        }
        .listRowBackground(Theme.pageBackground)
        .listRowSeparatorTint(Theme.hairline)
        .alignmentGuide(.listRowSeparatorLeading) { _ in 52 }
        .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            if store.typingAllowed, state == .needsInput, let tab = primary {
                Button { store.quickKey("enter", to: tab.id) } label: { Label("Enter", systemImage: "return") }
                    .tint(Theme.running)
                Button { store.quickKey("escape", to: tab.id) } label: { Label("Esc", systemImage: "escape") }
                    .tint(Theme.idle)
            }
        }
        .swipeActions(edge: .leading) {
            if store.typingAllowed, state != .idle, state != .none, let tab = primary {
                Button { store.quickKey("ctrl+c", to: tab.id) } label: { Label("Interrupt", systemImage: "stop.fill") }
                    .tint(Theme.danger)
            }
        }
        .onChange(of: workspace.agent) { _, _ in answered = nil }
        .accessibilityElement(children: .contain)
    }

    private var statusLabel: String? {
        switch state {
        case .needsInput: return "Needs you"
        case .running: return "Working"
        case .idle: return "Idle"
        case .none: return nil
        }
    }

    /// What the agent last said; for a menu, just its question (the choices are buttons).
    private var preview: String? {
        if let q = prompt?.question { return q }
        return workspace.cleanSummary
    }

    @ViewBuilder
    private var meta: some View {
        let folder = workspace.folder.flatMap { $0 == workspace.displayTitle ? nil : $0 }
        if workspace.gitBranch != nil || folder != nil || showServer {
            HStack(spacing: 10) {
                if showServer, let server = workspace.server {
                    Label(server, systemImage: "server.rack")
                }
                if let branch = workspace.gitBranch {
                    Label(branch, systemImage: "arrow.triangle.branch")
                }
                if let folder {
                    Label(folder, systemImage: "folder")
                }
            }
            .labelStyle(MetaLabelStyle())
            .font(.caption)
            .foregroundStyle(Theme.tertiaryText)
            .lineLimit(1)
        }
    }

    private var tabChips: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(workspace.openableTabs) { tab in
                    Button { open(tab.id) } label: {
                        HStack(spacing: 5) {
                            Image(systemName: tab.symbol)
                                .font(.system(size: 10, weight: .semibold))
                            Text(tab.detail(in: workspace).map { "\(tab.kindLabel) · \($0)" } ?? tab.kindLabel)
                                .lineLimit(1)
                        }
                        .font(.caption.weight(.medium))
                        .foregroundStyle(Theme.softText)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .background(Theme.raisedBackground, in: Capsule())
                    }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Open \(tab.kindLabel)")
                }
            }
        }
        .padding(.top, 4)
    }

    private func answers(_ prompt: PromptOptions, _ tab: Tab) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(prompt.options) { o in
                    Button {
                        UIImpactFeedbackGenerator(style: .light).impactOccurred()
                        store.quickSend(o.key, to: tab.id)
                        answered = o.key
                    } label: {
                        HStack(spacing: 6) {
                            Text(o.key)
                                .font(Theme.mono(11, weight: .bold))
                                .foregroundStyle(Theme.secondaryText)
                            Text(o.label).lineLimit(1)
                            if answered == o.key {
                                Image(systemName: "checkmark").font(.system(size: 10, weight: .bold))
                            }
                        }
                        .font(.footnote.weight(.medium))
                        .foregroundStyle(Theme.primaryText)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                        .background(Theme.cardBackground, in: Capsule())
                        .overlay(Capsule().stroke(answered == o.key ? Theme.needsInput : Theme.hairline))
                    }
                    .buttonStyle(.borderless)
                    .disabled(answered != nil)
                    .accessibilityLabel("Answer \(o.key), \(o.label)")
                }
            }
        }
        .padding(.top, 6)
    }
}

// MARK: - Headers and chips

struct SectionHeader: View {
    let title: String
    let dot: Color
    let count: Int
    var status: String? = nil

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(dot).frame(width: 6, height: 6)
            Text(title)
                .font(.footnote.weight(.semibold))
                .foregroundStyle(Theme.softText)
                .lineLimit(1)
            if let status {
                Text(status)
                    .font(.caption)
                    .foregroundStyle(dot)
            }
            Spacer(minLength: 0)
            Text("\(count)")
                .font(.caption.monospacedDigit())
                .foregroundStyle(Theme.tertiaryText)
        }
        .textCase(nil)
        .padding(.horizontal, 4)
        .padding(.top, 10)
        .padding(.bottom, 2)
        .frame(maxWidth: .infinity)
        .background(Theme.pageBackground)
        .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
    }
}

struct ServerChip: View {
    let name: String
    let dot: Color?
    let count: Int
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                if let dot { Circle().fill(dot).frame(width: 6, height: 6) }
                Text(name)
                    .font(.footnote.weight(.medium))
                    .foregroundStyle(selected ? Theme.pageBackground : Theme.softText)
                    .lineLimit(1)
                if count > 0 {
                    Text("\(count)")
                        .font(.caption2.weight(.bold).monospacedDigit())
                        .foregroundStyle(selected ? Theme.pageBackground : Theme.needsInput)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
            .background(selected ? Theme.primaryText : Theme.raisedBackground, in: Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityLabel(count > 0 ? "\(name), \(count) need you" : name)
    }
}

/// A small symbol and its text, tighter than the system label.
struct MetaLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            configuration.icon.imageScale(.small)
            configuration.title
        }
    }
}
