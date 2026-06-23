//
//  GlmFileSystem.swift
//  The `FSUnaryFileSystem` that loads a git-lazy-mount workspace as a volume.
//  The mount "resource" is a path URL (FSSupportsPathURLs); its path identifies
//  the registered mountpoint, which the FFI opens via the daemon Controller.
//

import Foundation
import FSKit

@objc
final class GlmFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {

    /// The security-scoped resource currently loaded (released on unload).
    private var resource: FSPathURLResource?

    override init() {
        super.init()
        Logger.glm.debug("GlmFileSystem init")
    }

    func loadResource(resource: FSResource,
                      options: FSTaskOptions,
                      replyHandler: @escaping (FSVolume?, (any Error)?) -> Void) {
        guard let urlResource = resource as? FSPathURLResource else {
            Logger.glm.error("loadResource: not an FSPathURLResource")
            return replyHandler(nil, POSIXError(.EINVAL))
        }
        // This module doesn't format; reject the force option like Apple's sample.
        for opt in options.taskOptions where opt.contains("-f") {
            return replyHandler(nil, POSIXError(.ENOTSUP))
        }
        guard urlResource.url.startAccessingSecurityScopedResource() else {
            Logger.glm.error("loadResource: can't access security-scoped resource")
            return replyHandler(nil, POSIXError(.EACCES))
        }
        self.resource = urlResource

        // The FFI opens the registered workspace for this mountpoint.
        let config: [String: String] = ["mountpoint": urlResource.url.path]
        guard let json = try? JSONSerialization.data(withJSONObject: config) else {
            urlResource.url.stopAccessingSecurityScopedResource()
            self.resource = nil
            return replyHandler(nil, POSIXError(.EINVAL))
        }
        let handle: OpaquePointer? = json.withUnsafeBytes { raw in
            glm_fskit_open(raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
        guard let handle else {
            let msg = glmLastErrorMessage()
            Logger.glm.error("glm_fskit_open failed: \(msg, privacy: .public)")
            urlResource.url.stopAccessingSecurityScopedResource()
            self.resource = nil
            return replyHandler(nil, POSIXError(.EIO))
        }

        let volumeName = FSFileName(string: urlResource.url.lastPathComponent + "_glm")
        self.containerStatus = .ready
        Logger.glm.info("loaded volume for \(urlResource.url.path, privacy: .public)")
        return replyHandler(GlmVolume(handle: handle, volumeName: volumeName), nil)
    }

    func unloadResource(resource: FSResource,
                        options: FSTaskOptions,
                        replyHandler reply: @escaping ((any Error)?) -> Void) {
        guard let urlResource = resource as? FSPathURLResource,
              let loaded = self.resource, loaded.url == urlResource.url else {
            return reply(POSIXError(.EINVAL))
        }
        loaded.url.stopAccessingSecurityScopedResource()
        self.resource = nil
        return reply(nil)
    }

    func probeResource(resource: FSResource,
                       replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        guard let urlResource = resource as? FSPathURLResource else {
            return replyHandler(nil, POSIXError(.ENODEV))
        }
        let name = urlResource.url.lastPathComponent + "_glm"
        let containerID = FSContainerIdentifier(uuid: UUID())
        return replyHandler(.usable(name: name, containerID: containerID), nil)
    }
}
