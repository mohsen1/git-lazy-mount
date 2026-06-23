//
//  glm_fskit.h
//  C ABI exported by the `glm-fskit-ffi` Rust static library (crates/fskit-ffi).
//  Imported into Swift via the bridging header so the FSKit extension can drive
//  the shared git-lazy-mount engine. Keep in lockstep with crates/fskit-ffi/src/lib.rs.
//

#ifndef GLM_FSKIT_H
#define GLM_FSKIT_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

/// Opaque handle to an opened workspace + FskitOps (see `glm_fskit_open`).
typedef struct GlmHandle GlmHandle;

/// C-ABI snapshot of a file's attributes (neutral; spec §28).
typedef struct GlmAttr {
    uint64_t ino;
    uint64_t generation;
    uint64_t size;
    uint32_t kind; // 0=file, 1=dir, 2=symlink, 3=gitlink/submodule
    uint32_t mode; // POSIX st_mode (type + perms)
} GlmAttr;

/// Per-entry callback for `glm_fskit_enumerate`. Return false to stop early.
typedef bool (*GlmEnumerateCallback)(void *ctx,
                                     const uint8_t *name_ptr,
                                     size_t name_len,
                                     const GlmAttr *attr);

/// Open the workspace registered at a mountpoint; NULL on failure (see
/// `glm_fskit_last_error`). `config` is UTF-8 JSON:
/// `{"mountpoint":"…","data_root":"…","volume":"case_sensitive"}`.
GlmHandle *glm_fskit_open(const uint8_t *config_ptr, size_t config_len);

/// Free a handle from `glm_fskit_open`.
void glm_fskit_close(GlmHandle *h);

/// Copy the last error message (UTF-8) into `buf`; returns its full length.
size_t glm_fskit_last_error(uint8_t *buf, size_t cap);

/// Operations — each returns a POSIX errno (0 = success).
int32_t glm_fskit_lookup(GlmHandle *h, uint64_t parent_ino,
                         const uint8_t *name_ptr, size_t name_len, GlmAttr *out);
int32_t glm_fskit_getattr(GlmHandle *h, uint64_t ino, GlmAttr *out);
int32_t glm_fskit_read(GlmHandle *h, uint64_t ino, uint64_t offset,
                       uint8_t *buf, size_t cap, size_t *out_len);
int32_t glm_fskit_readlink(GlmHandle *h, uint64_t ino,
                           uint8_t *buf, size_t cap, size_t *out_len);
void glm_fskit_forget(GlmHandle *h, uint64_t ino, uint64_t n);
int32_t glm_fskit_enumerate(GlmHandle *h, uint64_t ino, void *ctx,
                            GlmEnumerateCallback cb);
int32_t glm_fskit_create(GlmHandle *h, uint64_t parent_ino,
                         const uint8_t *name_ptr, size_t name_len,
                         bool executable, GlmAttr *out);
int32_t glm_fskit_symlink(GlmHandle *h, uint64_t parent_ino,
                          const uint8_t *name_ptr, size_t name_len,
                          const uint8_t *target_ptr, size_t target_len,
                          GlmAttr *out);
int32_t glm_fskit_write(GlmHandle *h, uint64_t ino, uint64_t offset,
                        const uint8_t *data_ptr, size_t data_len,
                        uint32_t *out_written);
int32_t glm_fskit_truncate(GlmHandle *h, uint64_t ino, uint64_t len);
int32_t glm_fskit_set_executable(GlmHandle *h, uint64_t ino, bool executable);
int32_t glm_fskit_remove(GlmHandle *h, uint64_t parent_ino,
                         const uint8_t *name_ptr, size_t name_len);
int32_t glm_fskit_rename(GlmHandle *h, uint64_t parent_ino,
                         const uint8_t *name_ptr, size_t name_len,
                         uint64_t new_parent_ino,
                         const uint8_t *new_name_ptr, size_t new_name_len);

/// The root inode number (FSKit/FuseOps convention).
uint64_t glm_fskit_root_ino(void);

#endif /* GLM_FSKIT_H */
