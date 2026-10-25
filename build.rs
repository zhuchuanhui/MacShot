use std::{env, fs, path::PathBuf, process::Command};

fn swift_runtime_path(developer_dir: &str) -> Option<PathBuf> {
    let swift_lib_dir =
        PathBuf::from(developer_dir).join("Toolchains/XcodeDefault.xctoolchain/usr/lib");
    let mut candidates = vec![swift_lib_dir.join("swift/macosx")];
    if let Ok(entries) = fs::read_dir(&swift_lib_dir) {
        candidates.extend(entries.flatten().map(|entry| entry.path().join("macosx")));
    }
    candidates
        .into_iter()
        .find(|path| path.join("libswift_Concurrency.dylib").is_file())
}

fn main() {
    println!("cargo:rerun-if-changed=assets/MacShot-Icon.ico");

    let target = env::var("TARGET").unwrap_or_default();
    if target.ends_with("-apple-darwin") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        if let Ok(output) = Command::new("xcode-select").arg("-p").output()
            && output.status.success()
        {
            let developer_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some(swift_runtime) = swift_runtime_path(&developer_dir) {
                println!(
                    "cargo:rustc-link-arg=-Wl,-rpath,{}",
                    swift_runtime.display()
                );
            }
        }
    }
    if !target.ends_with("-windows-gnu") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon_path = manifest_dir.join("assets/MacShot-Icon.ico");
    let script_path = out_dir.join("macshot-icon.rc");
    let resource_path = out_dir.join("macshot-icon.res");
    let escaped_icon_path = icon_path.to_string_lossy().replace('\\', "\\\\");
    fs::write(&script_path, format!("1 ICON \"{escaped_icon_path}\"\n"))
        .expect("failed to create Windows icon resource script");

    let windres = if target.starts_with("x86_64-") {
        "x86_64-w64-mingw32-windres"
    } else {
        "windres"
    };
    let status = Command::new(windres)
        .args(["-O", "coff", "-i"])
        .arg(&script_path)
        .arg("-o")
        .arg(&resource_path)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {windres}: {error}"));
    assert!(status.success(), "Windows icon resource compilation failed");

    println!(
        "cargo:rustc-link-arg-bin=macshot={}",
        resource_path.display()
    );
}
