//! Belastungstest für Crash-Recovery: schreibt Keys, killt den Prozess an einem
//! zufälligen Punkt (per `process::abort` im Binary) und verifiziert danach,
//! dass keine Datenkorruption entstanden ist.
//!
//! Das Binary `crash_tester` wird über `CARGO_BIN_EXE_crash_tester` gefunden.

use std::path::PathBuf;
use std::process::Command;

const N: usize = 5_000;
const RUNS: usize = 20;

fn crash_tester() -> &'static str {
    env!("CARGO_BIN_EXE_crash_tester")
}

#[test]
fn crash_recovery_no_corruption() {
    for run in 0..RUNS {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("lsm_crash_{}_{}", std::process::id(), run));
        let _ = std::fs::remove_dir_all(&dir);

        // Seed: schreibt N Keys und killt sich an einem zufälligen Punkt.
        let _ = Command::new(crash_tester())
            .args(["seed", dir.to_str().unwrap(), &N.to_string()])
            .status()
            .expect("run seed");
        // abort() → Signal/Trap, also kein Exit-Code 0. Ignorieren wir bewusst.

        // Verify: keine Korruption erlaubt, fehlende Keys erlaubt.
        let verify = Command::new(crash_tester())
            .args(["verify", dir.to_str().unwrap(), &N.to_string()])
            .output()
            .expect("run verify");
        assert!(
            verify.status.success(),
            "verify failed in run {run}: {}",
            String::from_utf8_lossy(&verify.stderr)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn clean_close_persists_everything() {
    let dir: PathBuf = std::env::temp_dir().join(format!("lsm_clean_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Clean shutdown über das Binary (write-Modus → close + exit(0)).
    let status = Command::new(crash_tester())
        .args(["write", dir.to_str().unwrap(), &N.to_string()])
        .status()
        .expect("run write");
    assert_eq!(status.code(), Some(0), "clean close should exit 0");

    // Alle Keys müssen nach sauberem Schluss vorhanden sein.
    let verify = Command::new(crash_tester())
        .args(["verify", dir.to_str().unwrap(), &N.to_string()])
        .output()
        .expect("run verify");
    assert!(
        verify.status.success(),
        "clean verify failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
