//! The child of a step of `owlshift watch --drain` in the tests (OWL-399):
//! `owlshift-step-child <PROJECT> <TICKET> <ENVELOPE>` does what
//! `owlshift continue <TICKET> --step <ENVELOPE>` does, on the project whose
//! paths the file `PROJECT` keeps ([`ChildProject`]), through the same
//! entry, `owlshift_runner::step::child`. As `owlshift` does, it first
//! makes the process trees it starts stop with it, however it ends. When
//! `OWLSHIFT_STEP_CHILD_PID` names a file, it writes its process id there
//! first.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use jiff::Timestamp;
use owlshift_contracts::step::StepEnvelope;
use owlshift_platform::process;
use owlshift_runner::events::printable;
use owlshift_runner::step;
use owlshift_testkit::step_child::ChildProject;

fn main() -> ExitCode {
    process::stop_trees_on_signal().expect("the signal policy");
    #[cfg(unix)]
    process::stop_trees_when_killed().expect("the sentinel");
    if let Some(file) = std::env::var_os("OWLSHIFT_STEP_CHILD_PID") {
        let file = PathBuf::from(file);
        let partial = file.with_extension("partial");
        std::fs::write(&partial, std::process::id().to_string()).unwrap();
        std::fs::rename(&partial, &file).unwrap();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [project, ticket, envelope] = args.as_slice() else {
        eprintln!("usage: owlshift-step-child <PROJECT> <TICKET> <ENVELOPE>");
        return ExitCode::from(2);
    };
    let envelope = match StepEnvelope::parse(envelope) {
        Ok(envelope) if envelope.ticket.as_str() == ticket => envelope,
        Ok(_) => {
            eprintln!("the envelope is another ticket's");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("the step's envelope: {error}");
            return ExitCode::from(2);
        }
    };
    let project = ChildProject::load(Path::new(project));
    // A run slot first, as `owlshift continue --step` takes one (OWL-409):
    // with none free, the "no slot" record is the step's outcome.
    let _slot = match project.machine_slots() {
        Some(slots) => {
            match step::take_slot(&slots, &project.dirs(), &envelope, Timestamp::now()) {
                Ok(Some(slot)) => Some(slot),
                Ok(None) => return ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("the machine's run slots: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };
    let written = project.with_on_demand(&mut io::stdout(), |on_demand, sink| {
        step::child(on_demand, &envelope, sink, &mut |outcome, _| {
            let said = match outcome {
                Ok(delivered) => delivered.to_string(),
                Err(stop) => stop.to_string(),
            };
            println!("{}", printable(&said));
        })
    });
    match written {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("the step's outcome record could not be written: {error}");
            ExitCode::FAILURE
        }
    }
}
