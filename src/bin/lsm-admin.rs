//! myLSM-DB Admin-CLI (v1.0 + v1.1): inspect / stats / compact / gc / backup / restore.
//!
//! Öffnet die Datenbank unter `--dir` (Default `.`) und führt den Befehl aus.
//! `inspect`/`stats` sind rein lesend (kein Flush/Drop-Mutation); `compact`/
//! `gc` mutieren gezielt und bleiben beim etablierten Durability-Vertrag.
//! `backup`/`restore` folgen dem Backup/Restore-Vertrag (Phase H Zweig A).
//!
//! Exit-Codes: 0 = ok, 1 = Laufzeitfehler (Open/Operation), 2 = Aufruffehler.

// Stil-Lints bewusst erlaubt (siehe Begründung in src/lib.rs); betrifft nur
// Kosmetik, keine Korrektheit. `cargo clippy -- -D warnings` bleibt sonst aktiv.
#![allow(
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::needless_range_loop,
    clippy::get_first,
    clippy::ptr_arg,
    clippy::unnecessary_map_or,
    clippy::unnecessary_sort_by
)]

use std::env;
use std::path::PathBuf;
use std::process;

use my_lsm_db::Database;

fn main() {
    let args: Vec<String> = env::args().collect();
    if let Err(code) = run(&args) {
        process::exit(code);
    }
}

fn run(args: &[String]) -> Result<(), i32> {
    let mut dir = PathBuf::from(".");
    let mut command: Option<&str> = None;
    let mut positionals: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "inspect" | "stats" | "compact" | "gc" | "backup" | "restore" => {
                if command.is_some() {
                    eprintln!("error: multiple commands given");
                    return Err(2);
                }
                command = Some(&args[i]);
            }
            "--dir" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --dir requires a value");
                    return Err(2);
                }
                dir = PathBuf::from(&args[i]);
            }
            other => {
                if other.starts_with("--") {
                    eprintln!("error: unknown argument: {other}");
                    return Err(2);
                }
                positionals.push(other.to_string());
            }
        }
        i += 1;
    }

    let command = match command {
        Some(c) => c,
        None => {
            eprintln!(
                "usage: lsm-admin <inspect|stats|compact|gc|backup|restore> [--dir <path>] <args...>"
            );
            return Err(2);
        }
    };

    match command {
        "inspect" => cmd_inspect(&dir),
        "stats" => cmd_stats(&dir),
        "compact" => cmd_compact(&dir),
        "gc" => cmd_gc(&dir),
        "backup" => {
            let dest = match positionals.get(0) {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("error: backup requires <dest>");
                    return Err(2);
                }
            };
            cmd_backup(&dir, &dest)
        }
        "restore" => {
            let src = match positionals.get(0) {
                Some(s) => PathBuf::from(s),
                None => {
                    eprintln!("error: restore requires <src> <dest>");
                    return Err(2);
                }
            };
            let dest = match positionals.get(1) {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("error: restore requires <src> <dest>");
                    return Err(2);
                }
            };
            cmd_restore(&src, &dest)
        }
        _ => unreachable!(),
    }
}

fn open_ro(dir: &PathBuf) -> Result<Database, i32> {
    Database::open(dir).map_err(|e| {
        eprintln!("error: {e}");
        1
    })
}

fn cmd_inspect(dir: &PathBuf) -> Result<(), i32> {
    let db = open_ro(dir)?;
    let summary = db.manifest_summary();

    println!("format-version: {}", db.format_version());
    println!("manifest:");
    println!("  next-table-id: {}", summary.next_table_id);
    println!("  level-count: {}", summary.level_count);
    println!("  levels:");
    for (lvl, ids) in summary.levels.iter().enumerate() {
        if lvl == 1 {
            // L1 wird separat über `segments` dargestellt.
            continue;
        }
        println!("    L{lvl}: {ids:?}");
    }
    println!("  segments: {}", summary.segments.len());
    for s in &summary.segments {
        println!(
            "    file {}: records={} min={} max={}",
            s.file_id,
            s.records,
            hex(&s.min_key),
            hex(&s.max_key),
        );
    }

    println!("tables:");
    match db.table_infos() {
        Ok(tables) => {
            for t in &tables {
                println!(
                    "  [L{}] id={} path={} records={} keys={} size={}",
                    t.level,
                    t.id,
                    t.path.display(),
                    t.num_records,
                    t.key_bounds_hex,
                    t.size_bytes,
                );
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            return Err(1);
        }
    }

    // Rein lesend schließen: kein Flush, keine WAL-Sync (keine Mutation).
    db.close_without_flush();
    Ok(())
}

fn cmd_stats(dir: &PathBuf) -> Result<(), i32> {
    let db = open_ro(dir)?;
    let tables = db.table_infos().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    let summary = db.manifest_summary();

    let l0: usize = summary.levels.get(0).map(|v| v.len()).unwrap_or(0);
    let l1 = tables.iter().filter(|t| t.level == 1).count();
    let l0_bytes: u64 = tables
        .iter()
        .filter(|t| t.level == 0)
        .map(|t| t.size_bytes)
        .sum();
    let l1_bytes: u64 = tables
        .iter()
        .filter(|t| t.level == 1)
        .map(|t| t.size_bytes)
        .sum();
    let o = db.options_view();

    println!("db-size: {} bytes", db.db_size());
    println!("sstable-count: {}", tables.len());
    println!("L0: tables={l0} bytes={l0_bytes}");
    println!("L1: tables={l1} bytes={l1_bytes}");
    println!("wal-size: {} bytes", db.wal_size());
    println!(
        "options: l0-compact-threshold={} segment-max-records={}",
        o.l0_compact_threshold, o.segment_max_records,
    );

    db.close_without_flush();
    Ok(())
}

fn cmd_compact(dir: &PathBuf) -> Result<(), i32> {
    let mut db = open_ro(dir)?;
    let before = db.table_count();
    let seg_count = db.compact_full().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    // Durability: manifest ist in compact_full bereits committet; close()
    // persitiert den (leeren) Rest sauber.
    db.close().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    println!("compacted: {} segments (was {before} sstables)", seg_count);
    Ok(())
}

fn cmd_gc(dir: &PathBuf) -> Result<(), i32> {
    let mut db = open_ro(dir)?;
    let removed = db.gc().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    db.close().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    println!("gc: removed {removed} orphan sst files");
    Ok(())
}

fn cmd_backup(dir: &PathBuf, dest: &PathBuf) -> Result<(), i32> {
    let mut db = open_ro(dir)?;
    let n = db.backup(dest).map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    // Durability: backup hat bereits geflusht; close() persitiert den Rest sauber.
    db.close().map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    println!("backup: wrote {n} files to {}", dest.display());
    Ok(())
}

fn cmd_restore(src: &PathBuf, dest: &PathBuf) -> Result<(), i32> {
    let n = Database::restore(src, dest).map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;
    println!("restore: wrote {n} files to {}", dest.display());
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
