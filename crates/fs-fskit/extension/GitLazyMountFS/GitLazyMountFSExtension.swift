//
//  GitLazyMountFSExtension.swift
//  The ExtensionKit entry point: declares the FSKit unary-file-system module
//  (spec §41). The system instantiates `GlmFileSystem` to serve a volume.
//

import Foundation
import FSKit

@main
struct GitLazyMountFSExtension: UnaryFileSystemExtension {
    var fileSystem: FSUnaryFileSystem & FSUnaryFileSystemOperations {
        GlmFileSystem()
    }
}
