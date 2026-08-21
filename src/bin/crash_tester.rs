//! Crash-Test-Harness für die v0.1.1-Engine.
//!
//! Zwei Modi:
//!   `crash_tester seed  <dir> <n>`   → schreibt `n` Keys und killt sich dabei
//!                                       an einem zufälligen Punkt (abort).
//!   `crash_tester verify <dir> <n>`  → öffnet die DB und prüft die Invariante.
//!
//! Der Loop über viele Runs (mit zufälligen Kill-Punkten) passiert im Test
//! `tests/crash.rs` via `std::process::Command`.
// Stil-Lints bewusst erlaubt (siehe Begründung in src/lib.rs); betrifft nur
// Kosmetik, keine Korrektheit.
#![allow(
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::needless_range_loop,
    clippy::manual_is_multiple_of,
    clippy::get_first,
    clippy::ptr_arg,
    clippy::unnecessary_map_or,
    clippy::unnecessary_sort_by
)]

use my_lsm_db::{Database, Options};
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
        "seedc" => seed_compaction(dir, n),
        "write" => write_all(dir, n),
        "verify" => verify(dir, n),
        "verifyc" => verify_compaction(dir, n),
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
        ..Options::default()
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
        ..Options::default()
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

/// Compaction-schwerer Seed: niedriger L0-Schwellwert + Updates + Deletes,
/// sodass Flush UND Compaction (Flatten/Konsolidierung) während der Schreibphase
/// ablaufen. Der `abort` kann an jedem Punkt innerhalb der Compaction-Sequenz
/// landen → deckt die drei Crash-Fenster ab (SSTable geschrieben ohne Manifest-
/// Commit; Manifest-COMMIT mit alten Dateien; Manifest-COMMIT nach Datei-Löschung).
fn seed_compaction(dir: &std::path::Path, n: usize) {
    let opts = Options {
        memtable_limit: 256 * 1024, // häufige Flushes
        l0_compact_threshold: 2,    // Compaction bereits nach 2 L0-Tabellen
        ..Options::default()
    };
    let mut db = Database::open_with(dir, opts).expect("open");

    let rnd = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let kill_at = rnd % (n as u64);

    for i in 0..n {
        let key = format!("key-{:08}", i % 2000).into_bytes();
        match i % 5 {
            0 => {
                db.delete(&key).expect("delete");
            }
            _ => {
                let val = format!("value-{}", i).into_bytes();
                db.put(&key, &val).expect("put");
            }
        }
        // Flush alle 8 Keys, um L0 zu füllen → triggert kompakt beim Schwellwert.
        // (Nicht pro Key: das wäre 100k fsync in debug; alle 8 reicht, um Flush
        // und Compaction zuverlässig über die Schreibphase zu streuen.)
        if i % 8 == 0 {
            db.flush().expect("flush");
        }

        if i as u64 == kill_at {
            process::abort();
        }
    }
    process::exit(0);
}

/// Verifiziert den compaction-schweren Seed: Jeder vorhandene Key muss einen
/// Wert tragen, der zu diesem Key tatsächlich geschrieben wurde (keine
/// Korruption). Da Keys überschrieben werden, ist jede zu `i % 2000 == k` und
/// `i % 5 != 0` geschriebene `value-{i}` ein gültiger Wert. Zusätzlich wird
/// geprüft, dass keine Manifest-Referenz auf eine fehlende Datei zeigt.
fn verify_compaction(dir: &std::path::Path, n: usize) {
    // Gültige Werte pro Key-Index (deterministisch aus der Seed-Sequenz).
    let mut valid: std::collections::HashMap<u32, Vec<Vec<u8>>> = std::collections::HashMap::new();
    for i in 0..n {
        if i % 5 == 0 {
            continue; // delete → kein Wert
        }
        let k = (i % 2000) as u32;
        valid
            .entry(k)
            .or_default()
            .push(format!("value-{}", i).into_bytes());
    }
    for vals in valid.values_mut() {
        vals.sort();
        vals.dedup();
    }

    let mut db = Database::open(dir).expect("reopen");

    // Keine Manifest-Referenz auf fehlende Dateien.
    for id in db.table_ids() {
        let p = dir.join(format!("{:06}.sst", id));
        if !p.exists() {
            eprintln!("CORRUPTION: manifest refs missing file {p:?}");
            process::exit(1);
        }
    }

    let rows = db.scan(None, None).expect("scan");
    for (key, val) in &rows {
        let s = String::from_utf8_lossy(key);
        let num: u32 = s
            .trim_start_matches("key-")
            .parse()
            .expect("parse key number");
        let Some(vals) = valid.get(&num) else {
            eprintln!("CORRUPTION: unexpected key {s:?}");
            process::exit(1);
        };
        if let Some(v) = val
            && !vals.contains(v)
        {
            eprintln!("CORRUPTION: key {s:?} has foreign value {v:?}");
            process::exit(1);
        }
    }
    println!("verifyc OK ({n} keys, {} rows)", rows.len());
    process::exit(0);
}

fn verify(dir: &std::path::Path, n: usize) {
    let mut db = Database::open(dir).expect("reopen");
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
