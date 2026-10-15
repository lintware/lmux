import SwiftUI
import UIKit

/// The lmux palette: quiet graphite with one muted terracotta accent. Matches
/// `app/src/theme.rs` (dark); the light side is a neutral off-white with white cards.
///
/// Every colour is defined here once and follows the system appearance. Views use
/// these tokens and never build their own `Color` values.
enum Theme {
    // Surfaces
    static let pageBackground = dynamic(dark: 0x20201E, light: 0xFAF9F6)
    static let cardBackground = dynamic(dark: 0x282825, light: 0xFFFFFF)
    static let raisedBackground = dynamic(dark: 0x30302B, light: 0xEEECE6)
    static let hairline = dynamic(dark: 0x393934, light: 0xE5E3DD)
    /// The selected segment of a switch: a step above `raisedBackground`.
    static let selectedFill = dynamic(dark: 0x3A3834, light: 0xFFFFFF)

    // Text
    static let primaryText = dynamic(dark: 0xE8E5DE, light: 0x1C1B19)
    static let softText = dynamic(dark: 0xC9C5BC, light: 0x3A3833)
    static let secondaryText = dynamic(dark: 0x99948A, light: 0x6B6760)
    static let tertiaryText = dynamic(dark: 0x6E6A63, light: 0x9C988F)

    // Brand
    static let accent = dynamic(dark: 0xC26B48, light: 0xB25A37)
    static let onAccent = Color.white
    static let flame = dynamic(dark: 0xC98A68, light: 0xA4532F)

    // Status
    static let running = dynamic(dark: 0x82B58E, light: 0x3E8A55)
    static let runningBg = dynamic(dark: 0x1C2A20, light: 0xE3F0E6)
    static let needsInput = dynamic(dark: 0xD6AA68, light: 0x96620F)
    static let needsInputBg = dynamic(dark: 0x2E2619, light: 0xF7EEDD)
    static let idle = secondaryText
    static let idlePillBg = raisedBackground
    static let danger = dynamic(dark: 0xD06E5A, light: 0xB0442E)

    // Terminal stays dark in both appearances, like the share page.
    static let terminalBackground = dynamic(dark: 0x0C0C0B, light: 0x161514)
    static let terminalText = Color(hex: 0xDEDBD3)

    // Chat
    static let userBubble = raisedBackground
    static let userBubbleText = primaryText
    static let codeBackground = dynamic(dark: 0x0C0C0B, light: 0xF0EEEA)
    static let composerField = cardBackground

    /// JetBrains Mono when the bundled font registered, else SF Mono.
    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        if let name = monoFontName { return .custom(name, size: size).weight(weight) }
        return .system(size: size, weight: weight, design: .monospaced)
    }

    private static let monoFontName: String? = {
        ["JetBrainsMono-Regular", "JetBrains Mono", "JetBrainsMono"].first { UIFont(name: $0, size: 12) != nil }
    }()

    private static func dynamic(dark: UInt32, light: UInt32) -> Color {
        Color(UIColor { $0.userInterfaceStyle == .dark ? UIColor(hex: dark) : UIColor(hex: light) })
    }
}

/// How a workspace's `agent` state looks in the list and the detail view.
enum AgentState: String {
    case needsInput = "needs_input"
    case running
    case idle
    case none

    init(_ raw: String?) { self = AgentState(rawValue: raw ?? "") ?? .none }

    /// Lower sorts first: whatever needs the user floats to the top.
    var rank: Int {
        switch self {
        case .needsInput: return 0
        case .running: return 1
        case .idle: return 2
        case .none: return 3
        }
    }

    var label: String? {
        switch self {
        case .needsInput: return "Needs input"
        case .running: return "Working"
        case .idle: return "Idle"
        case .none: return nil
        }
    }

    var foreground: Color {
        switch self {
        case .needsInput: return Theme.needsInput
        case .running: return Theme.running
        case .idle, .none: return Theme.idle
        }
    }

    var background: Color {
        switch self {
        case .needsInput: return Theme.needsInputBg
        case .running: return Theme.runningBg
        case .idle, .none: return Theme.idlePillBg
        }
    }
}

struct StatusPill: View {
    let state: AgentState

    var body: some View {
        if let label = state.label {
            HStack(spacing: 5) {
                Circle().fill(state.foreground).frame(width: 6, height: 6)
                Text(label)
            }
            .font(.caption2.weight(.semibold))
            .foregroundStyle(state.foreground)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(state.background, in: Capsule())
            .accessibilityElement(children: .combine)
        }
    }
}

private extension UIColor {
    convenience init(hex: UInt32) {
        self.init(
            red: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: 1
        )
    }
}

private extension Color {
    init(hex: UInt32) { self.init(UIColor(hex: hex)) }
}
