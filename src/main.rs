use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "scar",
    version,
    about = "Decompile and compile Apple Assets.car files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a summary of a .car file (header, key format, renditions).
    Info {
        /// Path to Assets.car
        car: PathBuf,
        /// List every rendition
        #[arg(long)]
        renditions: bool,
    },
    /// Extract a .car into a directory of PNGs/SVGs plus manifest.json.
    Decompile {
        /// Path to Assets.car
        car: PathBuf,
        /// Output directory (created if missing)
        #[arg(short, long)]
        out: PathBuf,
        /// Store every payload verbatim (byte-exact round-trip, no decoding)
        #[arg(long)]
        raw: bool,
        /// Skip preview PNGs (atlas crops, deepmap2/rle previews); faster, and the
        /// output still repacks with plain `compile`. Previews can't be edited.
        #[arg(long, conflicts_with = "raw")]
        no_previews: bool,
    },
    /// Build a .car from a decompiled directory (manifest.json + assets).
    Compile {
        /// Directory containing manifest.json
        dir: PathBuf,
        /// Output .car path
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Author a decompiled directory from a plain folder of images (no
    /// existing .car required); the output is ready for `compile`.
    Pack {
        /// Input folder (plain PNGs and/or *.imageset bundles)
        input: PathBuf,
        /// Output directory (manifest.json + renditions/…), ready for `compile`
        #[arg(short, long)]
        out: PathBuf,
        /// Target platform string (e.g. "ios")
        #[arg(long, default_value = "ios")]
        platform: String,
        /// Deployment platform version (e.g. "15.0")
        #[arg(long, default_value = "15.0")]
        platform_version: String,
    },
    /// Duplicate a named asset (facet + all its renditions) inside a
    /// decompiled directory, e.g. to author an alternate app icon.
    CloneAsset {
        /// Decompiled directory (manifest.json + assets)
        dir: PathBuf,
        /// Name of the existing asset (facet) to clone
        #[arg(long)]
        from: String,
        /// Name of the new asset
        #[arg(long)]
        to: String,
        /// PNG to install as the pixels of the clone's bitmap renditions
        #[arg(long)]
        image: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Info { car, renditions } => scar::decompile::info(&car, renditions),
        Command::Decompile {
            car,
            out,
            raw,
            no_previews,
        } => scar::decompile::decompile_with(
            &car,
            &out,
            &scar::decompile::DecompileOptions {
                raw,
                skip_previews: no_previews,
            },
        ),
        Command::Compile { dir, out } => scar::compile::compile(&dir, &out),
        Command::Pack {
            input,
            out,
            platform,
            platform_version,
        } => scar::authoring::pack(
            &input,
            &out,
            &scar::authoring::PackOptions {
                platform,
                platform_version,
            },
        ),
        Command::CloneAsset {
            dir,
            from,
            to,
            image,
        } => scar::authoring::clone_asset(&dir, &from, &to, image.as_deref()),
    }
}
