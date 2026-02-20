# Changelog

All notable changes will be documented in this file.

## 0.7.0 - 2026-02-20

- BREAKING: `Vfs::lock`, `Vfs::unlock`, and `Vfs::check_reserved_lock` no longer have default implementations and must be implemented by every `Vfs`.

## 0.6.0 - 2026-02-19

Added `check_reserved_lock` to Vfs. This method allows you to inform SQLite if any threads or processes currently hold a lock on the specified file. It is recommended to implement this method if you also implement the lock and unlock methods.

## 0.5.0 - 2025-11-26

- BREAKING: remove register_logger method from Vfs
- return SqliteLogger instance from register_dynamic/register_static

## 0.4.1 - 2025-06-19

- expose SqliteApi in public API

## 0.4.0 - 2025-06-19

- relax min SQLite version to 3.43.0

## 0.3.1 - 2025-06-16

- dependency bump

## 0.3.0 - 2025-05-26

- `register_dynamic` and `register_static` now require the VFS name to be passed in as a CString.

## 0.2.0 - 2025-05-19

- `PragmaErr` now requires an explicit error code and external construction.

## 0.1.2 - 2025-04-09

- updating dependencies

## 0.1.1 - 2025-04-09

- bug: support cross-compilation to arm

## 0.1.0 - 2025-03-29

- Initial release
