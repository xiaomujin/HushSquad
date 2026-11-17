//! 从 `docs/proto/hushsquad-control-v1.proto` 生成控制面 Rust 类型。
//!
//! 用 `protox`（纯 Rust 的 protobuf 编译器）而不是让 `prost-build` 去调 `protoc`：
//! 这样新机器和 CI 都不需要额外装一个二进制，构建可复现。
//! `skip_protoc_run()` 表示「描述符集合我已经给你了，别再自己起 protoc」。

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_dir = manifest.join("../../docs/proto");
    let proto_file = proto_dir.join("hushsquad-control-v1.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    let descriptors = protox::compile([&proto_file], [&proto_dir])
        .expect("编译 docs/proto/hushsquad-control-v1.proto 失败");

    prost_build::Config::new()
        .skip_protoc_run()
        .compile_fds(descriptors)
        .expect("生成控制面 Rust 类型失败");
}
