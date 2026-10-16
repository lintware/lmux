import Foundation
import Security
import os

/// One saved connection: a share's session cookie and where it works, plus what
/// the app last saw there (the gateway's name and its servers) for the
/// connections list. The cookie is a bearer credential for terminals, so the
/// list lives in the Keychain (this device only, after first unlock).
///
/// Rollover: the share sends a successor credential over the live socket. It
/// is saved as `nextCookie` first, then acknowledged, then promoted to
/// `cookie`; until then the old one still works, so a crash or a lost
/// connection at any step loses nothing.
struct SavedSession: Codable, Equatable, Identifiable {
    let id: UUID
    /// The share's public address. It follows the share when its tunnel
    /// restarts under a new one (the `endpoints` frame).
    var origin: URL
    var cookie: String
    var expires: Date
    /// A successor credential not yet acknowledged.
    var nextCookie: String?
    var nextExpires: Date?
    /// Every address of the share, the ones to try first first (tailnet,
    /// then the public tunnel), as the share last said.
    var endpoints: [URL]?
    /// The gateway or Mac's host name, from the share's tree.
    var name: String?
    /// The servers behind it when last connected.
    var servers: [String]?
    var lastUsed: Date?

    init(origin: URL, cookie: String, expires: Date) {
        id = UUID()
        self.origin = origin
        self.cookie = cookie
        self.expires = expires
    }

    // Sessions saved before the connections list had no id or name.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(UUID.self, forKey: .id) ?? UUID()
        origin = try c.decode(URL.self, forKey: .origin)
        cookie = try c.decode(String.self, forKey: .cookie)
        expires = try c.decode(Date.self, forKey: .expires)
        name = try c.decodeIfPresent(String.self, forKey: .name)
        servers = try c.decodeIfPresent([String].self, forKey: .servers)
        lastUsed = try c.decodeIfPresent(Date.self, forKey: .lastUsed)
        nextCookie = try c.decodeIfPresent(String.self, forKey: .nextCookie)
        nextExpires = try c.decodeIfPresent(Date.self, forKey: .nextExpires)
        endpoints = try c.decodeIfPresent([URL].self, forKey: .endpoints)
    }

    var isValid: Bool {
        let soon = Date().addingTimeInterval(5)
        return expires > soon || (nextCookie != nil && (nextExpires ?? .distantPast) > soon)
    }

    /// The credentials to try, the successor first: using it acknowledges it.
    var credentials: [String] {
        let soon = Date().addingTimeInterval(5)
        var list: [String] = []
        if let next = nextCookie, (nextExpires ?? .distantPast) > soon { list.append(next) }
        if expires > soon { list.append(cookie) }
        return list
    }

    /// Makes `token` the credential once the share has it acknowledged.
    mutating func promote(_ token: String) {
        guard nextCookie == token else { return }
        cookie = token
        expires = nextExpires ?? expires
        nextCookie = nil
        nextExpires = nil
    }
    var displayName: String { name ?? origin.host ?? "lmux" }
}

enum Keychain {
    private static let log = Logger(subsystem: "com.vas.lmux", category: "keychain")
    private static let service = "com.vas.lmux.share"
    /// The single session of earlier builds; moved into the list on first load.
    private static let legacyAccount = "session"
    private static let listAccount = "connections"

    static func loadAll() -> [SavedSession] {
        if let data = read(listAccount), let list = try? JSONDecoder().decode([SavedSession].self, from: data) {
            return list
        }
        guard let data = read(legacyAccount), let old = try? JSONDecoder().decode(SavedSession.self, from: data) else {
            return []
        }
        saveAll([old])
        delete(legacyAccount)
        return [old]
    }

    static func saveAll(_ list: [SavedSession]) {
        guard let data = try? JSONEncoder().encode(list) else { return }
        write(listAccount, data)
    }

    // MARK: - Items

    private static func read(_ account: String) -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status != errSecSuccess && status != errSecItemNotFound { log.error("keychain load failed: \(status, privacy: .public)") }
        return status == errSecSuccess ? result as? Data : nil
    }

    /// Replaces the item in place, so the saved list is never missing in
    /// between (a delete then add could lose every connection on a crash).
    private static func write(_ account: String, _ data: Data) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let update: [String: Any] = [kSecValueData as String: data]
        let updated = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if updated == errSecSuccess { return }
        if updated != errSecItemNotFound { log.error("keychain update failed: \(updated, privacy: .public)") }
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData as String: data,
        ]
        let status = SecItemAdd(item as CFDictionary, nil)
        if status != errSecSuccess { log.error("keychain save failed: \(status, privacy: .public)") }
    }

    private static func delete(_ account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
