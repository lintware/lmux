import SwiftUI
import UIKit

/// Scan the QR code that lmux shows ("Show QR…" or `lmux share`), or paste its link.
struct ConnectView: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    /// True when shown as a sheet over a live session.
    var isSheet = false

    @State private var pasted = ""
    @State private var error: String?
    @State private var scanKey = UUID()

    var body: some View {
        VStack(spacing: 18) {
            header
            ZStack {
                QRScannerView { code in handle(code) }
                    .id(scanKey)
                if store.phase == .redeeming {
                    Theme.pageBackground.opacity(0.75)
                    ProgressView("Connecting…").tint(Theme.accent).foregroundStyle(Theme.primaryText)
                }
            }
            .frame(maxWidth: .infinity)
            .aspectRatio(1, contentMode: .fit)
            .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous).stroke(Theme.hairline))

            if let message = error ?? expiredMessage {
                Text(message)
                    .font(.footnote)
                    .foregroundStyle(Theme.danger)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            pasteRow
            if !isSheet, !otherConnections.isEmpty { savedList }
            Spacer(minLength: 0)
        }
        .padding(20)
        .frame(maxWidth: .infinity)
        .background(Theme.pageBackground.ignoresSafeArea())
    }

    /// Saved connections other than the one that just expired.
    private var otherConnections: [SavedSession] {
        store.connections
            .filter { $0.id != store.activeID || $0.isValid }
            .sorted { ($0.lastUsed ?? .distantPast) > ($1.lastUsed ?? .distantPast) }
    }

    private var savedList: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Or pick a saved connection")
                .font(.footnote.weight(.semibold))
                .foregroundStyle(Theme.secondaryText)
            VStack(spacing: 0) {
                ForEach(otherConnections.prefix(4)) { c in
                    Button {
                        if c.isValid { store.switchTo(c.id) }
                    } label: {
                        ConnectionRow(connection: c, active: false, live: false)
                            .padding(.horizontal, 12)
                            .padding(.vertical, 4)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .disabled(!c.isValid)
                    .contextMenu {
                        Button(role: .destructive) { store.remove(c.id) } label: { Label("Remove", systemImage: "trash") }
                    }
                    if c.id != otherConnections.prefix(4).last?.id {
                        Rectangle().fill(Theme.hairline).frame(height: 1).padding(.leading, 60)
                    }
                }
            }
            .background(Theme.cardBackground, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        }
    }

    private var header: some View {
        VStack(spacing: 6) {
            HStack {
                Text("lmux")
                    .font(Theme.mono(24, weight: .bold))
                    .foregroundStyle(Theme.primaryText)
                Circle().fill(Theme.accent).frame(width: 8, height: 8)
                Spacer()
                if isSheet {
                    Button("Cancel") { dismiss() }.foregroundStyle(Theme.accent)
                }
            }
            Text(expiredMessage == nil ? "Scan a session's QR code" : "Scan a new QR code")
                .font(.title3.weight(.semibold))
                .foregroundStyle(Theme.primaryText)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text("On the Mac, open the lmux menu and choose Show QR…, or run `lmux share`.")
                .font(.footnote)
                .foregroundStyle(Theme.secondaryText)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var pasteRow: some View {
        HStack(spacing: 8) {
            TextField("", text: $pasted, prompt: Text("Paste a share link").foregroundStyle(Theme.tertiaryText))
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .keyboardType(.URL)
                .submitLabel(.go)
                .onSubmit { handle(pasted) }
                .foregroundStyle(Theme.primaryText)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
                .background(Theme.cardBackground, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(Theme.hairline))
            Button {
                if pasted.isEmpty, let clip = UIPasteboard.general.string { pasted = clip }
                handle(pasted)
            } label: {
                Text(pasted.isEmpty ? "Paste" : "Open")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(Theme.onAccent)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(Theme.accent, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
            .disabled(store.phase == .redeeming)
        }
    }

    private var expiredMessage: String? {
        if case let .expired(message) = store.phase { return message }
        return nil
    }

    private func handle(_ text: String) {
        guard let link = ShareLink(text) else {
            error = "That is not an lmux share link. It looks like https://….trycloudflare.com/#…"
            scanKey = UUID()
            return
        }
        error = nil
        Task {
            if let message = await store.open(link: link) {
                error = message
                scanKey = UUID()
            } else {
                pasted = ""
                if isSheet { dismiss() }
            }
        }
    }
}
