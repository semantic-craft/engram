//! Read-only project instruction stewardship commands.

use anyhow::Result;

use crate::cli::{InstructionsArgs, InstructionsCommand};
use crate::instruction_steward::DoctorReport;

/// Run `engram instructions` without loading runtime configuration, starting
/// logging, or opening any Engram store.
pub fn run(args: InstructionsArgs) -> Result<()> {
    match args.command {
        InstructionsCommand::Doctor(args) => {
            let report = DoctorReport::inspect_current_repository()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                report.print_human();
            }
            Ok(())
        }
    }
}
