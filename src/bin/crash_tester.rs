//! Crash-Test-Harness für die v0.1.1-Engine.
//!
//! Zwei Modi:
//!   `crash_tester seed  <dir> <n>`   → schreibt `n` Keys und killt sich dabei
//!                                       an einem zufälligen Punkt (abort).
//!   `crash_tester verify <dir> <n>`  → öffnet die DB und prüft die Invariante.
//!
//! Der Loop über viele Runs (mit zufälligen Kill-Punkten) passiert im Test
//! `tests/crash.rs` via `std::process::Command`.
use lsm_db::{Database, Options};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

const KEYS: usize = 10_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: crash_tester <seed|verify> <dir> [n]");
        process::exit(2);
    }
    let mode = &args[1];
    let dir = std::path::Path::new(&args[2]);
    let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(KEYS);

    match mode.as_str() {
        "seed" => seed(dir, n),
        "write" => write_all(dir, n),
        "verify" => verify(dir, n),
        _ => {
            eprintln!("unknown mode {mode}");
            process::exit(2);
        }
    }
}

/// Schreibt `n` Keys und schließt sauber (kein Kill). Für den Clean-Shutdown-Test.
fn write_all(dir: &std::path::Path, n: usize) {
    let opts = Options {
        memtable_limit: 4 * 1024 * 1024,
        l0_compact_threshold: 4,
    };
    let mut db = Database::open_with(dir, opts).expect("open");
    for i in 0..n {
        let key = format!("key-{:08}", i).into_bytes();
        let val = format!("value-{}", i).into_bytes();
        db.put(&key, &val).expect("put");
    }
    db.close().expect("close");
    process::exit(0);
}

fn seed(dir: &std::path::Path, n: usize) {
    let opts = Options {
        memtable_limit: 4 * 1024 * 1024, // erzwingt Flushes während der Schreibphase
        l0_compact_threshold: 4,
    };
    let mut db = Database::open_with(dir, opts).expect("open");

    // Deterministischer Seed aus Systemzeit → unterschiedlicher Kill-Punkt pro Run.
    let rnd = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let kill_at = rnd % (n as u64); // irgendwo in der Mitte

    for i in 0..n {
        let key = format!("key-{:08}", i).into_bytes();
        let val = format!("value-{}", i).into_bytes();
        db.put(&key, &val).expect("put");

        // Zufälliger Kill-Punkt: hart abbrechen (kein Drop → kein Best-Effort-Flush).
        if i as u64 == kill_at {
            // Manchmal mitten in der Schleife, manchmal kurz vor einem Flush.
            if rnd % 2 == 0 {
                db.flush().expect("flush");
            }
            process::abort(); // simuliert einen Absturz / Stromausfall
        }
    }
    // Falls kein Kill ausgelöst wurde (n=0 o.ä.), sauber beenden.
    process::exit(0);
}

fn verify(dir: &std::path::Path, n: usize) {
    let mut db = Database::open(dir).expect("reopen");

    // Ein einziger Scan über alle Keys (statt n einzelner get) → O(n) statt O(n²).
    let rows = db.scan(None, None).expect("scan");

    // Invariante: Jeder Key, der existiert, muss den korrekten Wert haben.
    // Nach einem Absturz ist nicht garantiert, dass ALLE Keys vorhanden sind,
    // aber es darf NIE einen falschen Wert oder eine Datenkorruption geben.
    let mut expected: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("key-{:08}", i).into_bytes())
        .collect();
    expected.sort();
    expected.dedup();

    let mut ri = 0usize;
    for want in &expected {
        while ri < rows.len() && rows[ri].0.as_slice() < want.as_slice() {
            ri += 1;
        }
        if ri < rows.len() && rows[ri].0.as_slice() == want.as_slice() {
            let expect_val = key_to_value(want);
            if let Some(val) = &rows[ri].1 {
                if val != &expect_val {
                    eprintln!("CORRUPTION: key {:?} has wrong value", want);
                    process::exit(1);
                }
            }
            ri += 1;
        }
        // Key fehlt (Crash vor dem Write) → erlaubt, einfach weiter.
    }
    println!("verify OK ({n} keys checked)");
    process::exit(0);
}

fn key_to_value(key: &[u8]) -> Vec<u8> {
    // key = "key-%08i" → value = "value-%i" (ohne Null-Padding)
    let s = String::from_utf8_lossy(key);
    let num = s.trim_start_matches("key-");
    let parsed: u32 = num.parse().expect("parse key number");
    format!("value-{}", parsed).into_bytes()
}