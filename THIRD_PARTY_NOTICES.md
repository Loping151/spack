# Third-party notices

spack is licensed under the [MIT License](LICENSE). Its dependencies retain their respective licenses. `src-tauri/Cargo.lock` and `package-lock.json` record the exact direct and transitive dependency versions.

## Dependencies

- [Zstandard](https://github.com/facebook/zstd): compression library. See the project's [license](https://github.com/facebook/zstd/blob/dev/LICENSE).
- [BLAKE3](https://github.com/BLAKE3-team/BLAKE3): file integrity hashing. See the project's license files.
- [ppmd-rust](https://github.com/hasenbanck/ppmd-rust), version 1.5.0: PPMd7 implementation, licensed under CC0-1.0 OR MIT-0.
- [serde](https://github.com/serde-rs/serde) and [serde_json](https://github.com/serde-rs/json): serialization. MIT OR Apache-2.0.
- [Tauri](https://github.com/tauri-apps/tauri): desktop application framework. See the project's license files.
- [WebView2](https://learn.microsoft.com/microsoft-edge/webview2/concepts/distribution): system runtime used by the Windows interface, subject to Microsoft's distribution terms.

Media format handling is implemented independently in Rust. spack does not bundle, link or invoke FFmpeg.
