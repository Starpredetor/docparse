//! `docparse export`: replay the index as JSONL.

use std::io::{BufWriter, Write};

use docparse::index::Index;

use crate::ExportArgs;

/// Exit code: 0 exported (possibly nothing), 1 no usable index or I/O failure.
pub(crate) fn run(args: &ExportArgs) -> i32 {
    // Read path: opens the index without creating or migrating anything.
    let index = match Index::open_existing(&args.path, args.index_dir.as_deref()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut sink: Box<dyn Write> = match &args.output {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => Box::new(BufWriter::new(f)),
            Err(e) => {
                eprintln!("error: cannot write {}: {e}", path.display());
                return 1;
            }
        },
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    if let Err(e) = index.export(&mut sink) {
        eprintln!("error: {e}");
        return 1;
    }
    if let Err(e) = sink.flush() {
        eprintln!("error: could not flush output: {e}");
        return 1;
    }
    0
}
