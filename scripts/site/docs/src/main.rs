//! `owlshift-site-docs ROOT OUT`: renders the documentation of the repository
//! at ROOT into OUT/docs/, beside the page `scripts/site/build.sh` copied into
//! OUT. Exits 1, naming each error, when the documentation breaks a rule of
//! the site.

use std::path::Path;
use std::process::ExitCode;

use owlshift_site_docs::{PAGES, Source, render};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [root, out] = &args[..] else {
        eprintln!("Usage: owlshift-site-docs ROOT OUT");
        return ExitCode::from(2);
    };
    let root = Path::new(root);
    let index = match std::fs::read_to_string(root.join("site").join("index.html")) {
        Ok(index) => index,
        Err(e) => {
            eprintln!("site/index.html: {e}");
            return ExitCode::FAILURE;
        }
    };
    let source = Source {
        root,
        pages: PAGES,
        index: &index,
    };
    match render(&source, Path::new(out)) {
        Ok(pages) => {
            println!("Documentation: {pages} pages written to {out}/docs");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for error in errors {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}
