//
//  GlmItem.swift
//  An `FSItem` backed by a stable engine inode (not a file descriptor — all I/O
//  is routed through the FFI by inode, unlike Apple's passthrough sample).
//

import Foundation
import FSKit

final class GlmItem: FSItem {
    /// Stable engine inode (the identity used across all FFI calls).
    let ino: UInt64
    /// The item's last-known name (exact recorded bytes).
    var name: FSFileName
    /// File / directory / symlink.
    var itemType: FSItem.ItemType

    init(ino: UInt64, name: FSFileName, type: FSItem.ItemType) {
        self.ino = ino
        self.name = name
        self.itemType = type
        super.init()
    }
}
