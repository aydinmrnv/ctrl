import CtrlCore
import Foundation
import Security

/// Keeps the core's identity and paired-device keys in the Keychain, as
/// generic passwords under one service.
public final class KeychainSecretStore: SecretStore, Sendable {
    private let service: String

    public init(service: String = "app.ctrl.core") {
        self.service = service
    }

    private func base(_ key: String? = nil) -> [CFString: Any] {
        var query: [CFString: Any] = [kSecClass: kSecClassGenericPassword, kSecAttrService: service]
        if let key { query[kSecAttrAccount] = key }
        return query
    }

    public func get(key: String) throws -> Data? {
        var query = base(key)
        query[kSecReturnData] = true
        query[kSecMatchLimit] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        try check(status)
        return result as? Data
    }

    public func set(key: String, value: Data) throws {
        let update: [CFString: Any] = [kSecValueData: value]
        let status = SecItemUpdate(base(key) as CFDictionary, update as CFDictionary)
        if status != errSecItemNotFound {
            return try check(status)
        }
        var add = base(key)
        add[kSecValueData] = value
        add[kSecAttrAccessible] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        try check(SecItemAdd(add as CFDictionary, nil))
    }

    public func delete(key: String) throws {
        let status = SecItemDelete(base(key) as CFDictionary)
        if status != errSecItemNotFound { try check(status) }
    }

    public func keys() throws -> [String] {
        var query = base()
        query[kSecReturnAttributes] = true
        query[kSecMatchLimit] = kSecMatchLimitAll
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return [] }
        try check(status)
        let items = result as? [[CFString: Any]] ?? []
        return items.compactMap { $0[kSecAttrAccount] as? String }
    }

    private func check(_ status: OSStatus) throws {
        guard status == errSecSuccess else {
            let message = SecCopyErrorMessageString(status, nil) as String? ?? "OSStatus \(status)"
            throw CtrlError.SecretStore(message: message)
        }
    }
}
