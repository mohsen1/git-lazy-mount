//
//  GlmVolume.swift
//  An `FSVolume` whose operations are served by the shared git-lazy-mount engine
//  through the `glm-fskit-ffi` C ABI. Unlike Apple's passthrough sample (which
//  holds file descriptors), every operation is addressed by a stable inode and
//  delegated to the engine via the FFI handle.
//

import Foundation
import FSKit
import OSLog

/// Non-capturing C callback for `glm_fskit_enumerate`; packs one directory entry
/// per invocation into the FSKit packer carried in `ctx`.
private let glmEnumerateCallback: GlmEnumerateCallback = { ctxPtr, namePtr, nameLen, attrPtr in
    guard let ctxPtr, let attrPtr else { return true }
    let ctx = Unmanaged<GlmEnumContext>.fromOpaque(ctxPtr).takeUnretainedValue()
    ctx.index += 1
    if ctx.index <= ctx.startCookie { return true } // already delivered in a prior call
    let a = attrPtr.pointee
    let nameBytes: [UInt8] = namePtr != nil ? Array(UnsafeBufferPointer(start: namePtr, count: nameLen)) : []
    let packed = ctx.packer.packEntry(
        name: FSFileName.from(bytes: nameBytes),
        itemType: glmItemType(a.kind),
        itemID: FSItem.Identifier(rawValue: a.ino) ?? .invalid,
        nextCookie: FSDirectoryCookie(ctx.index),
        attributes: nil)
    if !packed { ctx.full = true; return false }
    return true
}

/// Heap context handed to `glmEnumerateCallback` through the FFI `ctx` pointer.
private final class GlmEnumContext {
    let packer: FSDirectoryEntryPacker
    let startCookie: UInt64
    var index: UInt64 = 0
    var full = false
    init(packer: FSDirectoryEntryPacker, startCookie: UInt64) {
        self.packer = packer
        self.startCookie = startCookie
    }
}

final class GlmVolume: FSVolume,
                       FSVolume.Operations,
                       FSVolume.ReadWriteOperations,
                       FSVolume.OpenCloseOperations {

    /// Opaque FFI handle to the opened workspace (owned; freed on deinit).
    private let handle: OpaquePointer
    /// The root directory item.
    let rootItem: GlmItem
    /// Inode → item cache (FSKit expects stable item identities).
    private var itemCache: [UInt64: GlmItem] = [:]
    private let itemCacheQueue = DispatchQueue(label: "com.thirdface.gitlazymount.itemcache")

    init(handle: OpaquePointer, volumeName: FSFileName) {
        self.handle = handle
        let rootIno = glm_fskit_root_ino()
        self.rootItem = GlmItem(ino: rootIno, name: FSFileName(string: "."), type: .directory)
        super.init(volumeID: FSVolume.Identifier(uuid: UUID()), volumeName: volumeName)
        itemCache[rootIno] = rootItem
        Logger.glm.info("GlmVolume created: \(volumeName.string ?? "?", privacy: .public)")
    }

    deinit {
        glm_fskit_close(handle)
    }

    // MARK: - Item cache / attribute helpers

    private func itemFor(ino: UInt64, name: FSFileName, kind: UInt32) -> GlmItem {
        itemCacheQueue.sync {
            if let existing = itemCache[ino] {
                existing.name = name
                return existing
            }
            let item = GlmItem(ino: ino, name: name, type: glmItemType(kind))
            itemCache[ino] = item
            return item
        }
    }

    /// Build a fully-populated `FSItem.Attributes` from the engine's snapshot.
    private func attributes(from a: GlmAttr) -> FSItem.Attributes {
        let attrs = FSItem.Attributes()
        attrs.type = glmItemType(a.kind)
        attrs.mode = a.mode & 0o7777
        attrs.size = a.size
        attrs.allocSize = a.size
        attrs.linkCount = 1
        attrs.fileID = FSItem.Identifier(rawValue: a.ino) ?? .invalid
        attrs.uid = getuid()
        attrs.gid = getgid()
        let epoch = timespec(tv_sec: 0, tv_nsec: 0)
        attrs.accessTime = epoch
        attrs.modifyTime = epoch
        attrs.changeTime = epoch
        attrs.birthTime = epoch
        return attrs
    }

    // MARK: - Volume capabilities

    var volumeStatistics: FSStatFSResult {
        let res = FSStatFSResult(fileSystemTypeName: "gitlazymount")
        res.blockSize = 4096
        res.ioSize = 65536
        res.totalBlocks = 1 << 20
        res.availableBlocks = 1 << 19
        res.freeBlocks = 1 << 19
        res.usedBlocks = res.totalBlocks - res.freeBlocks
        res.totalFiles = 1 << 20
        res.freeFiles = 1 << 19
        return res
    }

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsSymbolicLinks = true
        caps.supportsPersistentObjectIDs = true
        caps.caseFormat = .sensitive
        return caps
    }

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { false }
    var truncatesLongNames: Bool { false }

    func setVolumeName(_ name: FSFileName, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        replyHandler(name, nil)
    }

    // MARK: - FSVolume.Operations: lifecycle

    func activate(options: FSTaskOptions, replyHandler reply: @escaping (FSItem?, (any Error)?) -> Void) {
        reply(rootItem, nil)
    }

    func deactivate(options: FSDeactivateOptions = [], replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    // MARK: - FSVolume.Operations: attributes

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest,
                       of item: FSItem,
                       replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let g = item as? GlmItem else { return replyHandler(nil, POSIXError(.EINVAL)) }
        var a = GlmAttr(ino: 0, generation: 0, size: 0, kind: 0, mode: 0)
        let rc = glm_fskit_getattr(handle, g.ino, &a)
        if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        replyHandler(attributes(from: a), nil)
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest,
                       on item: FSItem,
                       replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let g = item as? GlmItem else { return replyHandler(nil, POSIXError(.EINVAL)) }
        if newAttributes.isValid(.size) {
            let rc = glm_fskit_truncate(handle, g.ino, newAttributes.size)
            if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        }
        if newAttributes.isValid(.mode) {
            let exec = (newAttributes.mode & 0o111) != 0
            let rc = glm_fskit_set_executable(handle, g.ino, exec)
            if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        }
        var a = GlmAttr(ino: 0, generation: 0, size: 0, kind: 0, mode: 0)
        let rc = glm_fskit_getattr(handle, g.ino, &a)
        if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        replyHandler(attributes(from: a), nil)
    }

    // MARK: - FSVolume.Operations: lookup / enumerate / reclaim

    func lookupItem(named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let dir = directory as? GlmItem else { return replyHandler(nil, nil, POSIXError(.EINVAL)) }
        var a = GlmAttr(ino: 0, generation: 0, size: 0, kind: 0, mode: 0)
        let rc = withBytes(name.rawBytes) { p, n in glm_fskit_lookup(handle, dir.ino, p, n, &a) }
        if rc != 0 { return replyHandler(nil, nil, glmPosixError(rc)) }
        replyHandler(itemFor(ino: a.ino, name: name, kind: a.kind), name, nil)
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        if let g = item as? GlmItem {
            glm_fskit_forget(handle, g.ino, 1)
            _ = itemCacheQueue.sync { itemCache.removeValue(forKey: g.ino) }
        }
        replyHandler(nil)
    }

    func readSymbolicLink(_ item: FSItem,
                          replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard let g = item as? GlmItem else { return replyHandler(nil, POSIXError(.EINVAL)) }
        var buf = [UInt8](repeating: 0, count: 4096)
        var outLen = 0
        let rc = buf.withUnsafeMutableBytes {
            glm_fskit_readlink(handle, g.ino, $0.bindMemory(to: UInt8.self).baseAddress, $0.count, &outLen)
        }
        if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        let n = min(outLen, buf.count)
        replyHandler(FSFileName.from(bytes: Array(buf[0..<n])), nil)
    }

    func enumerateDirectory(_ directory: FSItem,
                            startingAt cookie: FSDirectoryCookie,
                            verifier: FSDirectoryVerifier,
                            attributes: FSItem.GetAttributesRequest?,
                            packer: FSDirectoryEntryPacker,
                            replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void) {
        guard let dir = directory as? GlmItem else {
            return replyHandler(FSDirectoryVerifier(0), POSIXError(.EINVAL))
        }
        let ctx = GlmEnumContext(packer: packer, startCookie: cookie.rawValue)
        let ctxPtr = Unmanaged.passUnretained(ctx).toOpaque()
        let rc = glm_fskit_enumerate(handle, dir.ino, ctxPtr, glmEnumerateCallback)
        if rc != 0 { return replyHandler(FSDirectoryVerifier(0), glmPosixError(rc)) }
        replyHandler(FSDirectoryVerifier(0), nil)
    }

    // MARK: - FSVolume.Operations: create / remove / rename

    func createItem(named name: FSFileName,
                    type: FSItem.ItemType,
                    inDirectory directory: FSItem,
                    attributes newAttributes: FSItem.SetAttributesRequest,
                    replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let dir = directory as? GlmItem else { return replyHandler(nil, nil, POSIXError(.EINVAL)) }
        if type == .directory {
            // Git has no empty directories; the engine materializes a directory on
            // first child write. Explicit mkdir is a known FFI gap (issue #19).
            return replyHandler(nil, nil, POSIXError(.ENOTSUP))
        }
        let exec = newAttributes.isValid(.mode) ? (newAttributes.mode & 0o111) != 0 : false
        var a = GlmAttr(ino: 0, generation: 0, size: 0, kind: 0, mode: 0)
        let rc = withBytes(name.rawBytes) { p, n in glm_fskit_create(handle, dir.ino, p, n, exec, &a) }
        if rc != 0 { return replyHandler(nil, nil, glmPosixError(rc)) }
        replyHandler(itemFor(ino: a.ino, name: name, kind: a.kind), name, nil)
    }

    func createSymbolicLink(named name: FSFileName,
                            inDirectory directory: FSItem,
                            attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName,
                            replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let dir = directory as? GlmItem else { return replyHandler(nil, nil, POSIXError(.EINVAL)) }
        var a = GlmAttr(ino: 0, generation: 0, size: 0, kind: 0, mode: 0)
        let rc = withBytes(name.rawBytes) { np, nn in
            withBytes(contents.rawBytes) { tp, tn in
                glm_fskit_symlink(handle, dir.ino, np, nn, tp, tn, &a)
            }
        }
        if rc != 0 { return replyHandler(nil, nil, glmPosixError(rc)) }
        replyHandler(itemFor(ino: a.ino, name: name, kind: a.kind), name, nil)
    }

    func createLink(to item: FSItem,
                    named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        replyHandler(nil, POSIXError(.ENOTSUP)) // hard links unsupported (spec §41)
    }

    func removeItem(_ item: FSItem,
                    named name: FSFileName,
                    fromDirectory directory: FSItem,
                    replyHandler: @escaping ((any Error)?) -> Void) {
        guard let dir = directory as? GlmItem else { return replyHandler(POSIXError(.EINVAL)) }
        let rc = withBytes(name.rawBytes) { p, n in glm_fskit_remove(handle, dir.ino, p, n) }
        if rc != 0 { return replyHandler(glmPosixError(rc)) }
        replyHandler(nil)
    }

    func renameItem(_ item: FSItem,
                    inDirectory sourceDirectory: FSItem,
                    named sourceName: FSFileName,
                    to destinationName: FSFileName,
                    inDirectory destinationDirectory: FSItem,
                    overItem: FSItem?,
                    replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard let fromDir = sourceDirectory as? GlmItem,
              let toDir = destinationDirectory as? GlmItem else {
            return replyHandler(nil, POSIXError(.EINVAL))
        }
        let rc = withBytes(sourceName.rawBytes) { sp, sn in
            withBytes(destinationName.rawBytes) { dp, dn in
                glm_fskit_rename(handle, fromDir.ino, sp, sn, toDir.ino, dp, dn)
            }
        }
        if rc != 0 { return replyHandler(nil, glmPosixError(rc)) }
        replyHandler(destinationName, nil)
    }

    // MARK: - FSVolume.ReadWriteOperations

    func read(from item: FSItem,
              at offset: off_t,
              length: Int,
              into buffer: FSMutableFileDataBuffer,
              replyHandler: @escaping (Int, (any Error)?) -> Void) {
        guard let g = item as? GlmItem else { return replyHandler(0, POSIXError(.EINVAL)) }
        var outLen = 0
        let rc = buffer.withUnsafeMutableBytes { raw -> Int32 in
            let cap = min(length, raw.count)
            return glm_fskit_read(handle, g.ino, UInt64(max(offset, 0)),
                                  raw.bindMemory(to: UInt8.self).baseAddress, cap, &outLen)
        }
        if rc != 0 { return replyHandler(0, glmPosixError(rc)) }
        replyHandler(outLen, nil)
    }

    func write(contents: Data,
               to item: FSItem,
               at offset: off_t,
               replyHandler: @escaping (Int, (any Error)?) -> Void) {
        guard let g = item as? GlmItem else { return replyHandler(0, POSIXError(.EINVAL)) }
        var written: UInt32 = 0
        let rc = contents.withUnsafeBytes { raw -> Int32 in
            glm_fskit_write(handle, g.ino, UInt64(max(offset, 0)),
                            raw.bindMemory(to: UInt8.self).baseAddress, raw.count, &written)
        }
        if rc != 0 { return replyHandler(0, glmPosixError(rc)) }
        replyHandler(Int(written), nil)
    }

    // MARK: - FSVolume.OpenCloseOperations (stateless engine; nothing to pin)

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }
}
