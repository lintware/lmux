import Foundation
import Security
import os

/// One saved connection: a share's session cookie and where it works, plus what
/// the app last saw there (the gateway's name and its servers) for the
/// connections list. The cookie is a bearer credential for terminals, so the
/// list lives in the Keychain (this device only, after first unlock).
struct SavedSession: Codable, Equatable, Identifiable {
    let id: UUID
    let origin: URL
    var cookie: String
    var expires: Date
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
    }

    var isValid: Bool { expires > Date().addingTimeInterval(5) }
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

    private static func write(_ account: String, _ data: Data) {
        delete(account)
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
