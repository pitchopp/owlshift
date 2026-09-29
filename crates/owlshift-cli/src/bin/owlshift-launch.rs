//! `owlshift-launch`: starts an agent command inside the run's Job Object on
//! native Windows (OWL-71). See `owlshift_platform::launch`.

#[cfg(windows)]
fn main() {
    std::process::exit(owlshift_platform::launch::main());
}

/// Elsewhere the sandbox runs the command itself: Cargo cannot leave a
/// binary out on one system, so this one only says so.
#[cfg(not(windows))]
fn main() {
    eprintln!("owlshift-launch: it starts agent commands on native Windows only");
    std::process::exit(127);
}
