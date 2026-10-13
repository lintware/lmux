import SwiftUI

/// Every saved connection: tap one to switch to it, swipe to remove it, or scan
/// a new QR code to add one. An expired connection asks for its QR again.
struct ConnectionsView: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var showScanner = false

    private var sorted: [SavedSession] {
        store.connections.sorted { a, b in
            if (a.id == store.activeID) != (b.id == store.activeID) { return a.id == store.activeID }
            return (a.lastUsed ?? .distantPast) > (b.lastUsed ?? .distantPast)
        }
    }

    var body: some View {
        NavigationStack {
            List {
                if !store.connections.isEmpty {
                    Section {
                        ForEach(sorted) { c in
                            ConnectionRow(connection: c, active: c.id == store.activeID, live: store.isLive)
                                .contentShape(Rectangle())
                                .onTapGesture { pick(c) }
                                .listRowBackground(Theme.cardBackground)
                                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                                    Button(role: .destructive) { store.remove(c.id) } label: {
                                        Label("Remove", systemImage: "trash")
                                    }
                                }
                                .contextMenu {
                                    if c.id != store.activeID || !store.isLive {
                                        Button { pick(c) } label: {
                                            Label(c.isValid ? "Connect" : "Scan again", systemImage: c.isValid ? "arrow.right.circle" : "qrcode.viewfinder")
                                        }
                                    }
                                    Button(role: .destructive) { store.remove(c.id) } label: {
                                        Label("Remove", systemImage: "trash")
                                    }
                                }
                        }
                    } header: {
                        Text("Saved")
                    } footer: {
                        Text("Saved in this iPhone's Keychain. When a gateway restarts, its address changes: scan its QR code again and the old entry is replaced.")
                    }
                }
                Section {
                    Button { showScanner = true } label: {
                        Label("Add a connection", systemImage: "qrcode.viewfinder")
                            .font(.body.weight(.medium))
                    }
                    .listRowBackground(Theme.cardBackground)
                } footer: {
                    Text("On the Mac, open the lmux menu and choose Show QR…, or use the REMOTE button.")
                }
            }
            .listStyle(.insetGrouped)
            .scrollContentBackground(.hidden)
            .background(Theme.pageBackground.ignoresSafeArea())
            .navigationTitle("Connections")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }.fontWeight(.semibold)
                }
            }
            .toolbarBackground(Theme.pageBackground, for: .navigationBar)
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .sheet(isPresented: $showScanner) { ConnectView(isSheet: true) }
        .onChange(of: store.activeID) { _, _ in
            // A scan that added or refreshed a connection switched to it.
            if store.phase != .needsLink { dismiss() }
        }
    }

    private func pick(_ c: SavedSession) {
        guard c.isValid else {
            showScanner = true
            return
        }
        UISelectionFeedbackGenerator().selectionChanged()
        store.switchTo(c.id)
        dismiss()
    }
}

struct ConnectionRow: View {
    let connection: SavedSession
    let active: Bool
    let live: Bool

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: (connection.servers?.count ?? 0) > 1 ? "server.rack" : "laptopcomputer")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(connection.isValid ? Theme.softText : Theme.tertiaryText)
                .frame(width: 36, height: 36)
                .background(Theme.raisedBackground, in: Circle())
            VStack(alignment: .leading, spacing: 2) {
                Text(connection.displayName)
                    .font(.body.weight(.semibold))
                    .foregroundStyle(connection.isValid ? Theme.primaryText : Theme.secondaryText)
                    .lineLimit(1)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(Theme.tertiaryText)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            status
        }
        .padding(.vertical, 4)
        .accessibilityElement(children: .combine)
    }

    private var detail: String {
        if let servers = connection.servers, servers.count > 1 {
            return servers.joined(separator: ", ")
        }
        return connection.origin.host ?? ""
    }

    @ViewBuilder
    private var status: some View {
        if !connection.isValid {
            Text("Expired")
                .font(.caption.weight(.medium))
                .foregroundStyle(Theme.danger)
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .background(Theme.danger.opacity(0.12), in: Capsule())
        } else if active {
            HStack(spacing: 5) {
                Circle().fill(live ? Theme.running : Theme.needsInput).frame(width: 6, height: 6)
                Text(live ? "Connected" : "Connecting")
            }
            .font(.caption.weight(.medium))
            .foregroundStyle(live ? Theme.running : Theme.needsInput)
        } else if let used = connection.lastUsed {
            Text(used, format: .relative(presentation: .named))
                .font(.caption)
                .foregroundStyle(Theme.tertiaryText)
        }
    }
}
