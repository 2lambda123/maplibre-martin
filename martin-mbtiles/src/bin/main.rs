use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    version,
    name = "mbtiles",
    about = "A utility to work with .mbtiles files content"
)]
pub struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Prints all values in the metadata table.
    #[command(name = "meta-all")]
    MetaAll {
        /// MBTiles file to read from
        file: PathBuf,
    },
    /// Gets a single value from metadata table.
    #[command(name = "meta-get")]
    MetaGetValue {
        /// MBTiles file to read a value from
        file: PathBuf,
    },
    /// Sets a single value in the metadata table, or deletes it if no value.
    #[command(name = "meta-set")]
    MetaSetValue {
        /// MBTiles file to modify
        file: PathBuf,
    },
    /// Copy tiles from one mbtiles file to another.
    Copy {
        /// MBTiles file to read from
        src_file: PathBuf,
        /// MBTiles file to write to
        dst_file: PathBuf,
    },
}

fn main() {
    let args = Args::parse();

    println!("Parsed args:\n");
    println!("{args:#?}");
    println!();
}
