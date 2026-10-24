use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VCPKG_TRIPLET: &str = "x64-windows-static-md";
const FFMPEG_RUNTIME_DLLS: &[&str] = &["avcodec-62.dll", "avutil-60.dll", "swresample-6.dll"];

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=native/pslc.c");
    println!("cargo:rerun-if-changed=native/pslc.h");
    println!("cargo:rerun-if-changed=native/CMakeLists.txt");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-env-changed=PSLC_VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=PSLC_FFMPEG_DIR");

    let vcpkg_root = env::var_os("PSLC_VCPKG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".tools/vcpkg"));
    let ffmpeg_dir = env::var_os("PSLC_FFMPEG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".tools/ffmpeg"));
    require(
        &vcpkg_root.join("scripts/buildsystems/vcpkg.cmake"),
        "vcpkg (run scripts/setup-deps.ps1)",
    );
    require(
        &ffmpeg_dir.join("include/libavcodec/avcodec.h"),
        "FFmpeg dev build (run scripts/setup-deps.ps1)",
    );

    if env::var_os("CMAKE").is_none()
        && which("cmake").is_none()
        && let Some(cmake) = visual_studio_cmake()
    {
        unsafe { env::set_var("CMAKE", cmake) };
    }

    let installed = vcpkg_root.join("installed");
    let protoc = find_first(&[
        installed.join("x64-windows/tools/protobuf/protoc.exe"),
        installed.join(format!("{VCPKG_TRIPLET}/tools/protobuf/protoc.exe")),
    ])
    .expect("protoc not found in vcpkg installation");
    let pkgconf = find_first(&[
        installed.join(format!("{VCPKG_TRIPLET}/tools/pkgconf/pkgconf.exe")),
        installed.join("x64-windows/tools/pkgconf/pkgconf.exe"),
    ])
    .expect("pkgconf not found in vcpkg installation");
    let mut path = env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    path.insert(0, protoc.parent().unwrap().to_path_buf());
    unsafe { env::set_var("PATH", env::join_paths(path).unwrap()) };
    let python = which("python")
        .or_else(|| which("py"))
        .expect("Python 3 is required to build chiaki-ng (nanopb generator)");

    // chiaki-ng patches curl with `git apply` inside the build directory; without a ceiling git would
    // find this repository above target/ and silently skip the patch.
    unsafe { env::set_var("GIT_CEILING_DIRECTORIES", &root) };
    let vs = visual_studio_install()
        .expect("Visual Studio Build Tools with the C++ workload are required");
    apply_msvc_environment(&vs);
    let ninja = vs.join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe");
    let clang_cl = env::var_os("PSLC_CLANG_CL")
        .map(PathBuf::from)
        .or_else(|| find_first(&[PathBuf::from(r"C:\Program Files\LLVM\bin\clang-cl.exe")]))
        .or_else(|| which("clang-cl"))
        .expect("clang-cl (LLVM) is required: chiaki-ng uses C99 variable-length arrays that MSVC cl.exe rejects");

    let dst = cmake::Config::new("native")
        .profile("Release")
        .generator("Ninja")
        .define("CMAKE_MAKE_PROGRAM", &ninja)
        .define("CMAKE_C_COMPILER", &clang_cl)
        .define("CMAKE_CXX_COMPILER", &clang_cl)
        .define(
            "CMAKE_TOOLCHAIN_FILE",
            vcpkg_root.join("scripts/buildsystems/vcpkg.cmake"),
        )
        .define("VCPKG_TARGET_TRIPLET", VCPKG_TRIPLET)
        .define("VCPKG_MANIFEST_MODE", "OFF")
        .define("FFMPEG_DIR", &ffmpeg_dir)
        .define("PROTOC", &protoc)
        .define("PKG_CONFIG_EXECUTABLE", &pkgconf)
        .define("PYTHON_EXECUTABLE", &python)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreadedDLL")
        .build();

    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("lib").display()
    );
    println!("cargo:rustc-link-lib=dylib=pslc_core");

    let profile_dir = out_profile_dir();
    let mut runtime: Vec<PathBuf> = vec![dst.join("bin/pslc_core.dll")];
    runtime.extend(
        FFMPEG_RUNTIME_DLLS
            .iter()
            .map(|dll| ffmpeg_dir.join("bin").join(dll)),
    );
    for dll in runtime {
        if !dll.exists() {
            panic!("missing runtime library {}", dll.display());
        }
        for target in [profile_dir.clone(), profile_dir.join("deps")] {
            let _ = fs::create_dir_all(&target);
            fs::copy(&dll, target.join(dll.file_name().unwrap())).expect("copy runtime dll");
        }
    }

    embed_resources(&root);
}

fn embed_resources(root: &Path) {
    let rc = root.join("assets/app.rc");
    if !rc.exists() {
        return;
    }
    let Some(rc_exe) = windows_sdk_tool("rc.exe") else {
        println!("cargo:warning=rc.exe not found, executable icon is not embedded");
        return;
    };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app.res");
    let status = Command::new(rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&out)
        .arg(&rc)
        .current_dir(root.join("assets"))
        .status()
        .expect("run rc.exe");
    if status.success() {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=rc.exe failed, executable icon is not embedded");
    }
}

fn windows_sdk_tool(name: &str) -> Option<PathBuf> {
    let bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(&bin)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|v| v.join("x64").join(name))
        .find(|p| p.exists())
}

fn out_profile_dir() -> PathBuf {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    out.ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR layout")
        .to_path_buf()
}

fn require(path: &Path, what: &str) {
    if !path.exists() {
        panic!("{what} not found: {}", path.display());
    }
}

fn find_first(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.exists()).cloned()
}

fn which(program: &str) -> Option<PathBuf> {
    let output = Command::new("where").arg(program).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| PathBuf::from(l.trim()))
        .find(|p| p.exists() && !p.to_string_lossy().contains("WindowsApps"))
}

fn visual_studio_install() -> Option<PathBuf> {
    let vswhere =
        PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    let output = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    let install = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!install.is_empty()).then(|| PathBuf::from(install))
}

fn visual_studio_cmake() -> Option<PathBuf> {
    let cmake = visual_studio_install()?
        .join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe");
    cmake.exists().then_some(cmake)
}

fn apply_msvc_environment(vs: &Path) {
    let vcvars = vs.join(r"VC\Auxiliary\Build\vcvars64.bat");
    let output = Command::new("cmd")
        .arg("/C")
        .arg(format!("call \"{}\" >nul && set", vcvars.display()))
        .output()
        .expect("run vcvars64.bat");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=')
            && (matches!(
                key.to_ascii_uppercase().as_str(),
                "PATH" | "INCLUDE" | "LIB" | "LIBPATH"
            ) || key.starts_with("VC")
                || key.starts_with("Windows")
                || key.starts_with("UCRT"))
        {
            unsafe { env::set_var(key, value) };
        }
    }
}
