use std::env;
use std::path::Path;

use super::path::{Dirs, RelPath};
use super::prepare::GitRepo;
use super::rustc_info::get_file_name;
use super::utils::hyperfine_command;

static SIMPLE_RAYTRACER_REPO: GitRepo = GitRepo::github(
    "ebobby",
    "simple-raytracer",
    "804a7a21b9e673a482797aa289a18ed480e4d813",
    "<none>",
);

pub(crate) fn benchmark(dirs: &Dirs) {
    benchmark_simple_raytracer(dirs);
}

fn benchmark_simple_raytracer(dirs: &Dirs) {
    if std::process::Command::new("hyperfine").output().is_err() {
        eprintln!("Hyperfine not installed");
        eprintln!("Hint: Try `cargo install hyperfine` to install hyperfine");
        std::process::exit(1);
    }

    if !SIMPLE_RAYTRACER_REPO.source_dir().to_path(dirs).exists() {
        SIMPLE_RAYTRACER_REPO.fetch(dirs);
    }

    let bench_runs = env::var("BENCH_RUNS").unwrap_or_else(|_| "10".to_string()).parse().unwrap();

    eprintln!("[BENCH COMPILE] ebobby/simple-raytracer");
    let cargo_clif =
        RelPath::DIST.to_path(dirs).join(get_file_name("cargo_clif", "bin").replace('_', "-"));
    let manifest_path = SIMPLE_RAYTRACER_REPO.source_dir().to_path(dirs).join("Cargo.toml");
    let target_dir = RelPath::BUILD.join("simple_raytracer").to_path(dirs);

    let clean_cmd = format!(
        "RUSTC=rustc cargo clean --manifest-path {manifest_path} --target-dir {target_dir}",
        manifest_path = manifest_path.display(),
        target_dir = target_dir.display(),
    );
    // FIXME apply -Cpanic=abort to cg_llvm compiled code
    let llvm_build_cmd = format!(
        "RUSTC=rustc cargo build -Zbuild-std=std --target aarch64-unknown-linux-gnu --manifest-path {manifest_path} --target-dir {target_dir} && (rm build/raytracer_cg_llvm || true) && ln build/simple_raytracer/aarch64-unknown-linux-gnu/debug/main build/raytracer_cg_llvm",
        manifest_path = manifest_path.display(),
        target_dir = target_dir.display(),
    );
    let llvm_build_opt_cmd = format!(
        "RUSTC=rustc cargo build -Zbuild-std=std --target aarch64-unknown-linux-gnu --release --manifest-path {manifest_path} --target-dir {target_dir} && (rm build/raytracer_cg_llvm_opt || true) && ln build/simple_raytracer/aarch64-unknown-linux-gnu/release/main build/raytracer_cg_llvm_opt",
        manifest_path = manifest_path.display(),
        target_dir = target_dir.display(),
    );
    let clif_build_cmd = format!(
        "RUSTC=rustc {cargo_clif} build --manifest-path {manifest_path} --target-dir {target_dir} && (rm build/raytracer_cg_clif || true) && ln build/simple_raytracer/debug/main build/raytracer_cg_clif",
        cargo_clif = cargo_clif.display(),
        manifest_path = manifest_path.display(),
        target_dir = target_dir.display(),
    );
    let clif_build_opt_cmd = format!(
        "RUSTC=rustc {cargo_clif} build --manifest-path {manifest_path} --target-dir {target_dir} --release && (rm build/raytracer_cg_clif_opt || true) && ln build/simple_raytracer/release/main build/raytracer_cg_clif_opt",
        cargo_clif = cargo_clif.display(),
        manifest_path = manifest_path.display(),
        target_dir = target_dir.display(),
    );

    hyperfine_command(
        0,
        1,
        Some(&clean_cmd),
        &[&llvm_build_cmd, &llvm_build_opt_cmd, &clif_build_cmd, &clif_build_opt_cmd],
        Path::new("."),
    );

    eprintln!("[BENCH RUN] ebobby/simple-raytracer");

    hyperfine_command(
        0,
        bench_runs,
        None,
        &[
            Path::new(".").join(get_file_name("raytracer_cg_llvm", "bin")).to_str().unwrap(),
            Path::new(".").join(get_file_name("raytracer_cg_llvm_opt", "bin")).to_str().unwrap(),
            Path::new(".").join(get_file_name("raytracer_cg_clif", "bin")).to_str().unwrap(),
            Path::new(".").join(get_file_name("raytracer_cg_clif_opt", "bin")).to_str().unwrap(),
        ],
        &RelPath::BUILD.to_path(dirs),
    );
}
