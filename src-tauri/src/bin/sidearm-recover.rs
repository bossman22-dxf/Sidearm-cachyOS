#[cfg(target_os = "linux")]
use std::io::{self, Write};

use std::{
    env,
    path::Path,
    process::ExitCode,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "Usage:\n  sidearm-recover --backup-onboard-memory <file>\n  sidearm-recover --validate-onboard-backup <file>\n  sidearm-recover --restore-onboard-backup <file>\n  sidearm-recover --apply-sidearm-mapping <complete-backup-file>"
        );
        return ExitCode::from(2);
    }

    let operation = args[1].as_str();
    let path = Path::new(&args[2]);

    #[cfg(target_os = "linux")]
    {
        match operation {
            "--backup-onboard-memory" => {
                match sidearm_lib::linux_onboard_backup::backup_connected_mouse(path) {
                    Ok(true) => {
                        println!("Complete backup created: {}", path.display());
                        ExitCode::SUCCESS
                    }
                    Ok(false) => {
                        eprintln!("Backup saved but incomplete; it cannot be used for mapping or restore.");
                        ExitCode::FAILURE
                    }
                    Err(error) => {
                        eprintln!("Backup failed: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
            "--validate-onboard-backup" => {
                match sidearm_lib::linux_onboard_backup::validate_backup_for_restore(path) {
                    Ok(record_count) => {
                        println!("Valid, complete, restorable backup: {record_count} raw records.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("Backup is not restorable: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
            "--apply-sidearm-mapping" => {
                println!("This will set the twelve thumb buttons to F13-F24 and keep the two side buttons as mouse buttons 4 and 5.");
                println!("A complete matching backup is required; if a write fails, Sidearm will attempt rollback.");
                println!("Type APPLY to continue:");
                print!("> ");
                if io::stdout().flush().is_err() {
                    eprintln!("Could not flush confirmation prompt.");
                    return ExitCode::FAILURE;
                }
                let mut confirmation = String::new();
                if io::stdin().read_line(&mut confirmation).is_err()
                    || confirmation.trim() != "APPLY"
                {
                    eprintln!("Mapping cancelled.");
                    return ExitCode::from(2);
                }
                match sidearm_lib::linux_onboard_backup::apply_sidearm_mapping(path) {
                    Ok(()) => {
                        println!("Sidearm mapping applied and every written record was verified.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("Mapping failed: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
            "--restore-onboard-backup" => {
                println!(
                    "This will overwrite the onboard button records on the connected Razer Naga V2 HyperSpeed."
                );
                println!("The backup must be complete and match the device. Type RESTORE to continue:");
                print!("> ");
                if io::stdout().flush().is_err() {
                    eprintln!("Could not flush confirmation prompt.");
                    return ExitCode::FAILURE;
                }
                let mut confirmation = String::new();
                if io::stdin().read_line(&mut confirmation).is_err()
                    || confirmation.trim() != "RESTORE"
                {
                    eprintln!("Restore cancelled.");
                    return ExitCode::from(2);
                }
                match sidearm_lib::linux_onboard_backup::restore_backup(path) {
                    Ok(()) => {
                        println!("All onboard records were restored and verified.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("Restore failed: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("Unknown operation: {operation}");
                ExitCode::from(2)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (operation, path);
        eprintln!("The onboard recovery utility is currently supported on Linux only.");
        ExitCode::from(2)
    }
}
