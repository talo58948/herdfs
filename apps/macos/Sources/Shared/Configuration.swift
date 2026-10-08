import Foundation

struct Configuration {
    private static let suite = "dev.herdfs.HerdFS"
    private static var defaults: UserDefaults {
        UserDefaults(suiteName: suite) ?? .standard
    }

    static var authority: String {
        get { defaults.string(forKey: "authority") ?? "" }
        set { defaults.set(newValue, forKey: "authority") }
    }

    static var issuer: String {
        get { defaults.string(forKey: "issuer") ?? "" }
        set { defaults.set(newValue, forKey: "issuer") }
    }

    static var clientID: String {
        get { defaults.string(forKey: "clientID") ?? "" }
        set { defaults.set(newValue, forKey: "clientID") }
    }
}
