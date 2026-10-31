# Local owner transport source notes (2026-10-29)

- Linux man-pages, unix(7), https://man7.org/linux/man-pages/man7/unix.7.html:
  pathname sockets use filesystem names and directory permissions; socket-file permission behavior
  is not uniform across Unix systems. Therefore access control includes a protected containing
  directory and a random capability, not only the socket file's mode. Linux also has an abstract
  namespace, which is avoided because it lacks filesystem directory access control.
- Rust std::fs::File::try_lock,
  https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock:
  the nonblocking exclusive lock refuses contention and releases when its file handles close.
  Therefore endpoint publication retains the lock for the listener lifetime and never unlinks the
  lock file during ordinary shutdown. This supplements rather than replaces canonical database
  ownership locking. The SDK now retains a separate private database-owner lease before opening
  an engine, while publication retains its own lock.

These sources support endpoint and lock semantics; admission constants are explicit resource
policy and are not performance measurements. Portability is only claimed for compiled/tested
shells. Linux, macOS and Windows share the protected-loopback implementation; native validation
is recorded separately from cross-compilation.

- Microsoft Winsock socket security,
  https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse:
  SO_REUSEADDR can make TCP delivery indeterminate; SO_EXCLUSIVEADDRUSE is set before bind.
  The Windows shell uses the latter through safe socket2 APIs.
- private-fs 0.2.1 API, https://docs.rs/private-fs/0.2.1/private_fs/, and downloaded crate source:
  PrivateDir creates and verifies current-token-user-only DACLs; operations remain relative to
  retained validated directory handles. PrivateFile exposes AsHandle, allowing a safe standard
  owned-handle duplicate for File::try_lock. Separate record publication avoids LockFileEx's
  exclusive byte-range read denial. Source inspection also establishes a boundary: Windows
  ancestor traversal rejects reparse points but does not validate ancestor DELETE_CHILD grants.
  The embedding host must protect the database's directory namespace.
- RFC 2104, https://www.rfc-editor.org/rfc/rfc2104.html: HMAC combines a secret key with a hash to
  authenticate messages. This protocol uses HMAC-SHA256 with distinct direction labels and
  fresh client/server nonces; capability bytes are never a wire field.
