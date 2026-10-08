import Darwin
import Foundation

func realHomeDirectory() -> URL {
    guard let entry = getpwuid(getuid()), let directory = entry.pointee.pw_dir else {
        return FileManager.default.homeDirectoryForCurrentUser
    }
    return URL(fileURLWithPath: String(cString: directory), isDirectory: true)
}
