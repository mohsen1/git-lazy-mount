//
//  GlmSupport.swift
//  Shared helpers: logging, errno→error mapping, FFI attribute/type bridging,
//  and `FSFileName` ⇄ raw bytes (exact recorded bytes, spec §41).
//

import Foundation
import FSKit
import OSLog

extension Logger {
    static let glm = Logger(subsystem: "com.thirdface.gitlazymount", category: "fskit")
}

/// Wrap a POSIX errno from the FFI as an `Error` the kernel understands.
func glmPosixError(_ code: Int32) -> POSIXError {
    POSIXError(POSIXError.Code(rawValue: code) ?? .EIO)
}

/// Read the FFI's thread-local last-error message (for diagnostics/logging).
func glmLastErrorMessage() -> String {
    var buf = [UInt8](repeating: 0, count: 1024)
    let n = buf.withUnsafeMutableBytes { glm_fskit_last_error($0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
    let len = min(Int(n), buf.count)
    return String(decoding: buf[0..<len], as: UTF8.self)
}

/// Map the FFI `kind` code to FSKit's item type.
func glmItemType(_ kind: UInt32) -> FSItem.ItemType {
    switch kind {
    case 1: return .directory
    case 2: return .symlink
    case 3: return .directory // gitlink/submodule surfaces as a directory
    default: return .file
    }
}

extension FSFileName {
    /// The exact recorded bytes of this name (never assume UTF-8; spec §41).
    var rawBytes: [UInt8] {
        if let d = self.data as Data? { return [UInt8](d) }
        if let s = self.string { return [UInt8](s.utf8) }
        return []
    }

    /// Build a name from raw bytes.
    static func from(bytes: [UInt8]) -> FSFileName {
        FSFileName(data: Data(bytes))
    }
}

/// Run `body` with a pointer to `bytes` (or a valid empty pointer).
func withBytes<R>(_ bytes: [UInt8], _ body: (UnsafePointer<UInt8>?, Int) -> R) -> R {
    if bytes.isEmpty {
        return body(nil, 0)
    }
    return bytes.withUnsafeBufferPointer { body($0.baseAddress, $0.count) }
}
