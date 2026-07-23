//! Shared integration-test helpers.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Minimal CARHEADER metadata for hand-authored test manifests (pack's values, minus the derived uuid).
#[allow(dead_code)] // each integration-test crate compiles its own copy
pub fn synthetic_car_info() -> scar::manifest::CarInfo {
    scar::manifest::CarInfo {
        coreui_version: 974,
        storage_version: 17,
        storage_timestamp: 0,
        main_version_string: "@(#)PROGRAM:CoreUI  PROJECT:CoreUI-974.1".to_string(),
        version_string: "scar test fixture".to_string(),
        uuid: "00112233445566778899aabbccddeeff".to_string(),
        associated_checksum: 0,
        schema_version: 2,
        color_space_id: 1,
        key_semantics: 2,
        key_format: scar::authoring::default_key_format(),
        metadata: None,
    }
}

/// Assert `assetutil -I` accepts a catalog (skips off-macOS): exits within the timeout (writer bugs show up
/// as hangs), no corrupt renditions, no BOM errors. "couldn't materialize" from minimal authored renditions is tolerated.
#[allow(dead_code)] // each integration-test crate compiles its own copy
pub fn assert_assetutil_accepts(car: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let assetutil = Path::new("/usr/bin/assetutil");
    if !assetutil.exists() {
        eprintln!(
            "assetutil not present, skipping acceptance check for {}",
            car.display()
        );
        return;
    }

    let mut child = Command::new(assetutil)
        .arg("-I")
        .arg(car)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning assetutil");

    // Drain pipes on threads: the JSON dump exceeds the pipe buffer, and an undrained pipe looks like a hang.
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        match child.try_wait().expect("waiting for assetutil") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "assetutil -I hung on {} (>120s); with this catalog size that is a structural bug, not slowness",
                    car.display()
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let stdout = stdout_thread.join().unwrap();
    let stderr = stderr_thread.join().unwrap();

    assert!(
        status.success(),
        "assetutil -I rejected {}: {status}\nstderr: {stderr}",
        car.display()
    );
    assert!(
        stdout.trim_start().starts_with('['),
        "assetutil -I produced no JSON for {}\nstderr: {stderr}",
        car.display()
    );
    assert!(
        !stdout.contains("Corrupt data"),
        "assetutil -I reports corrupt renditions in {} ({} occurrence(s))",
        car.display(),
        stdout.matches("Corrupt data").count()
    );
    assert!(
        !stderr.contains("can't get size of value"),
        "assetutil -I hit BOM tree-walk errors on {}:\n{stderr}",
        car.display()
    );
}
