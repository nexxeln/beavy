//! What the snapshot and trash commands share: finding the store, and the store's
//! place in results.

use std::io::{self, Write};
use std::path::PathBuf;

use beavy::output::{Format, Output};
use beavy::store::{self, Location, Selection, Store};
use serde::Serialize;

use crate::{Failure, Global, StoreArgs, working_directory};

/// A store resolved from `--cwd`, `--store`, and `BEAVY_STORE`.
pub struct Opened {
    pub store: Store,
    pub location: Location,
}

pub fn open(global: &Global, args: &StoreArgs) -> Result<Opened, Failure> {
    let cwd = working_directory(global)?;
    let location = store::locate(&cwd, args.store.as_deref(), |name| std::env::var_os(name))?;
    let mut context = Vec::new();
    for (option, value) in [("--cwd", &global.cwd), ("--store", &args.store)] {
        if let Some(value) = value {
            context.push(option.to_string());
            context.push(value.to_string_lossy().into_owned());
        }
    }
    let store = Store::at(location.path.clone()).with_context(context);
    Ok(Opened { store, location })
}

/// The `store` object in every result.
#[derive(Serialize)]
pub struct StoreView {
    pub path: String,
}

impl Opened {
    pub fn view(&self) -> StoreView {
        StoreView {
            path: self.location.path.to_string_lossy().into_owned(),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.location.path
    }

    /// Show the store as context on stderr when argv doesn't already name it.
    pub fn context<O: Write, E: Write>(&self, output: &mut Output<O, E>) -> io::Result<()> {
        if self.location.selection == Selection::Option || output.mode().format == Format::Json {
            return Ok(());
        }
        let mut stderr = output.stderr();
        stderr.row("Store", &self.location.path.to_string_lossy())?;
        stderr.blank()
    }
}
