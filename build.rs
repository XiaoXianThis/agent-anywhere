//! 构建脚本：在 release 构建时把 frontend/ 打包到 static/dist/，
//! debug 构建跳过（cargo run 会启动 vite dev 服务器，不需要预构建）。
//!
//! 跳过开关：环境变量 SKIP_FRONTEND_BUILD=1。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let frontend_dir = manifest_dir.join("frontend");

    println!("cargo:rerun-if-env-changed=SKIP_FRONTEND_BUILD");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-changed=build.rs");
    for rel in [
        "frontend/src",
        "frontend/index.html",
        "frontend/package.json",
        "frontend/package-lock.json",
        "frontend/vite.config.ts",
        "frontend/tsconfig.json",
        "frontend/tsconfig.app.json",
        "frontend/tsconfig.node.json",
    ] {
        println!("cargo:rerun-if-changed={rel}");
    }

    if std::env::var_os("DOCS_RS").is_some() {
        return;
    }
    if std::env::var_os("SKIP_FRONTEND_BUILD").is_some() {
        warn("SKIP_FRONTEND_BUILD 已设置，跳过前端构建");
        return;
    }
    if !frontend_dir.exists() {
        warn("未找到 frontend/ 目录，跳过前端构建");
        return;
    }

    // 仅 release 构建打包前端；debug 时由运行时 spawn vite dev 服务器。
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile != "release" {
        warn(&format!(
            "profile={profile}，跳过前端 production build（debug 运行时会启动 vite dev 服务器）",
        ));
        return;
    }

    if !frontend_dir.join("node_modules").exists() {
        warn("frontend/node_modules 不存在，正在执行 npm install ...");
        run(npm_cmd(), &["install"], &frontend_dir);
    }

    warn("正在执行 npm run build ...");
    run(npm_cmd(), &["run", "build"], &frontend_dir);
}

fn run(program: &str, args: &[&str], cwd: &Path) {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "无法执行 `{} {}`（请确认已安装 Node.js / npm）：{e}",
                program,
                args.join(" "),
            )
        });
    if !status.success() {
        panic!(
            "`{} {}` 失败 (exit={:?})",
            program,
            args.join(" "),
            status.code(),
        );
    }
}

fn warn(msg: &str) {
    println!("cargo:warning={msg}");
}

#[cfg(windows)]
fn npm_cmd() -> &'static str {
    "npm.cmd"
}

#[cfg(not(windows))]
fn npm_cmd() -> &'static str {
    "npm"
}
