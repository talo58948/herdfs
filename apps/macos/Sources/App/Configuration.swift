import Foundation

struct Configuration {
    static var authority: String {
        get { UserDefaults.standard.string(forKey: "authority") ?? "" }
        set { UserDefaults.standard.set(newValue, forKey: "authority") }
    }

    static var issuer: String {
        get { UserDefaults.standard.string(forKey: "issuer") ?? "" }
        set { UserDefaults.standard.set(newValue, forKey: "issuer") }
    }

    static var clientID: String {
        get { UserDefaults.standard.string(forKey: "clientID") ?? "" }
        set { UserDefaults.standard.set(newValue, forKey: "clientID") }
    }

    static var resource: String {
        get { UserDefaults.standard.string(forKey: "resource") ?? "" }
        set { UserDefaults.standard.set(newValue, forKey: "resource") }
    }
}
