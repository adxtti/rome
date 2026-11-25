//! The `rome` command line tool.

mod convert;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use u_rome_core::AssetKind;
use u_rome_index::{EntryId, EntryRow, Index, SearchFilters};
use u_rome_install::{EntryReader, ScanEvent, ScanSettings};

use convert::As;

#[derive(Parser)]
#[command(name = "rome", version, about = "Unofficial ROME command line tool")]
struct Cli {
    /// Index database. Defaults to the shared Studio index.
    #[arg(long, global = true)]
    index: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a game install folder into the index.
    Scan {
        /// The GTA VI install folder (the one containing the .rpf files).
        game: PathBuf,
        /// Skip BLAKE3 content hashing (faster, no duplicate detection).
        #[arg(long)]
        no_hash: bool,
        /// Worker threads (default: all cores).
        #[arg(long)]
        threads: Option<usize>,
        /// Rescan files the index already has.
        #[arg(long)]
        force: bool,
        /// Hash/name lists to import (TSV `0xHASH<TAB>name` or one name per line).
        #[arg(long = "names")]
        names: Vec<PathBuf>,
    },
    /// Show index totals.
    Stats,
    /// List indexed archives.
    Archives,
    /// List entries in one archive.
    Ls {
        /// Archive path relative to the install, as shown by `rome archives`.
        archive: String,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Search entries by name, 0xHASH or BLAKE3 prefix.
    Search {
        #[arg(default_value = "")]
        query: String,
        /// Filter by kind (archive, texture-store, texture-table, pso, meta, audio, video, mesh, xml, unknown).
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show everything known about one entry.
    Info { entry: i64 },
    /// Extract an entry, optionally converting it (PNG, glTF, Ogg, JSON).
    Extract {
        entry: i64,
        /// Output folder.
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// Write the raw decoded bytes instead of converting.
        #[arg(long)]
        raw: bool,
        /// Override the detected kind.
        #[arg(long = "as", value_enum, default_value_t = As::Auto)]
        as_: As,
    },
    /// Detect and convert a loose file (for example one written by `extract --raw`).
    Decode {
        file: PathBuf,
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        #[arg(long = "as", value_enum, default_value_t = As::Auto)]
        as_: As,
    },
    /// Print the asset kind detected from a file's leading bytes.
    Detect { path: PathBuf },
}

fn parse_kind(s: &str) -> Result<AssetKind> {
    Ok(match s.to_ascii_lowercase().replace('_', "-").as_str() {
        "archive" => AssetKind::Archive,
        "texture-store" | "texture" => AssetKind::TextureStore,
        "texture-table" => AssetKind::TextureTable,
        "pso" => AssetKind::Pso,
        "meta" | "map" => AssetKind::MetaResource,
        "audio" => AssetKind::Audio,
        "video" => AssetKind::Video,
        "mesh" => AssetKind::MeshBlob,
        "xml" => AssetKind::Xml,
        "unknown" => AssetKind::Unknown,
        other => bail!("unknown kind {other:?}"),
    })
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn print_row(r: &EntryRow) {
    let chain: Vec<String> = r.chain.iter().map(|o| format!("0x{o:X}")).collect();
    println!(
        "{:>9}  {:<14} {:>10}  {:<9} {}{}",
        r.id.0,
        r.kind.label(),
        human(r.size),
        format!("{:?}", r.codec),
        chain.join(" > "),
        if r.provisional { "  (provisional)" } else { "" }
    );
}

fn open_index(path: &std::path::Path) -> Result<Index> {
    if !path.exists() {
        bail!(
            "no index at {} (run `rome scan <game folder>` first)",
            path.display()
        );
    }
    Index::open(path).with_context(|| format!("opening index {}", path.display()))
}

fn reader_for(index_path: &std::path::Path) -> Result<EntryReader> {
    let root = u_rome_install::load_root(index_path).with_context(|| {
        format!(
            "index {} has no recorded install folder; rescan it",
            index_path.display()
        )
    })?;
    Ok(EntryReader::new(&root))
}

fn entry(index: &Index, id: i64) -> Result<EntryRow> {
    index
        .entry(EntryId(id))?
        .with_context(|| format!("no entry {id}"))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let index_path = cli.index.unwrap_or_else(u_rome_install::default_index_path);

    match cli.command {
        Command::Scan {
            game,
            no_hash,
            threads,
            force,
            names,
        } => {
            let settings = ScanSettings {
                hash: !no_hash,
                threads,
                force,
                name_lists: names,
                ..Default::default()
            };
            let cancel = AtomicBool::new(false);
            let started = Instant::now();
            let report = u_rome_install::scan_install(
                &game,
                &index_path,
                &settings,
                &cancel,
                &|ev| match ev {
                    ScanEvent::Discovered { files, total_bytes } => {
                        eprintln!("found {files} archives, {}", human(total_bytes))
                    }
                    ScanEvent::FileStarted {
                        index,
                        rel_path,
                        size,
                    } => {
                        eprintln!("[{}] scanning {rel_path} ({})", index + 1, human(size))
                    }
                    ScanEvent::FileDone { found, seconds, .. } => {
                        eprintln!("      {found} entries in {seconds:.1}s")
                    }
                    ScanEvent::FileSkipped {
                        rel_path, reason, ..
                    } => eprintln!("      skip {rel_path}: {reason}"),
                    ScanEvent::FileFailed {
                        rel_path, error, ..
                    } => eprintln!("      FAILED {rel_path}: {error}"),
                    ScanEvent::NamesImported { names } => eprintln!("imported {names} names"),
                    ScanEvent::Progress { .. } => {}
                },
            )?;
            println!(
                "scanned {} / skipped {} / failed {} of {} files, {} entries, {} names, {:.1}s{}",
                report.scanned,
                report.skipped,
                report.failed,
                report.files,
                report.entries,
                report.names,
                started.elapsed().as_secs_f64(),
                if report.cancelled { " (cancelled)" } else { "" }
            );
            println!("index: {}", index_path.display());
        }
        Command::Stats => {
            let index = open_index(&index_path)?;
            let s = index.stats()?;
            println!(
                "archives {} (done {}, partial {}, pending {})",
                s.archives, s.archives_done, s.archives_partial, s.archives_pending
            );
            println!(
                "entries {} ({} named, {} provisional), names {}",
                s.entries, s.named_entries, s.provisional_entries, s.names
            );
            println!(
                "decoded {}, stored {}",
                human(s.total_size),
                human(s.total_stored_len)
            );
            for k in &s.by_kind {
                println!(
                    "  {:<14} {:>9}  {:>10}",
                    k.kind.label(),
                    k.count,
                    human(k.size)
                );
            }
        }
        Command::Archives => {
            let index = open_index(&index_path)?;
            for a in index.archives()? {
                println!(
                    "{:>4}  {:<9} {:>10} {:>8} entries  {}",
                    a.id.0,
                    format!("{:?}", a.state),
                    human(a.size),
                    a.entry_count,
                    a.rel_path
                );
            }
        }
        Command::Ls {
            archive,
            limit,
            offset,
        } => {
            let index = open_index(&index_path)?;
            let a = index
                .archive_by_path(&archive)?
                .with_context(|| format!("no archive {archive:?}; see `rome archives`"))?;
            for r in index.entries_in(a.id, offset, limit)? {
                print_row(&r);
            }
        }
        Command::Search { query, kind, limit } => {
            let index = open_index(&index_path)?;
            let filters = SearchFilters {
                kind: kind.as_deref().map(parse_kind).transpose()?,
                archive: None,
            };
            for r in index.search(&query, &filters, limit, 0)? {
                print_row(&r);
            }
        }
        Command::Info { entry: id } => {
            let index = open_index(&index_path)?;
            let r = entry(&index, id)?;
            let arch = index.archive(r.archive_id)?;
            println!("entry       {}", r.id.0);
            println!(
                "archive     {}",
                arch.map(|a| a.rel_path).unwrap_or_default()
            );
            println!(
                "location    {}",
                r.chain
                    .iter()
                    .map(|o| format!("0x{o:X}"))
                    .collect::<Vec<_>>()
                    .join(" > ")
            );
            println!("kind        {}", r.kind.label());
            println!("codec       {:?} ({} pages)", r.codec, r.pages);
            println!(
                "size        {} decoded, {} stored",
                human(r.size),
                human(r.stored_len)
            );
            if let Some(h) = r.hash {
                println!(
                    "blake3      {}",
                    h.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
            }
            println!("provisional {}", r.provisional);
            for n in index.names_for_hash(r.name_hash.unwrap_or(0))? {
                println!("name        {}", n.name);
            }
            let head: String = r
                .head
                .iter()
                .take(32)
                .map(|b| format!("{b:02x} "))
                .collect();
            println!("head        {head}");
            for c in index.children(r.id)?.iter().take(20) {
                print_row(c);
            }
        }
        Command::Extract {
            entry: id,
            out,
            raw,
            as_,
        } => {
            let index = open_index(&index_path)?;
            let r = entry(&index, id)?;
            let reader = reader_for(&index_path)?;
            let bytes = reader
                .read(&index, &r)
                .with_context(|| format!("reading entry {id}"))?;
            let stem = format!("entry_{id}");
            let files = if raw {
                std::fs::create_dir_all(&out)?;
                let p = out.join(format!("{stem}.bin"));
                std::fs::write(&p, &bytes)?;
                vec![p]
            } else {
                convert::convert(&bytes, r.kind, as_, &out, &stem)?
            };
            for f in files {
                println!("{}", f.display());
            }
        }
        Command::Decode { file, out, as_ } => {
            let bytes =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "out".into());
            for f in convert::convert(&bytes, u_rome_core::detect(&bytes), as_, &out, &stem)? {
                println!("{}", f.display());
            }
        }
        Command::Detect { path } => {
            let data = std::fs::read(&path)?;
            println!("{}", u_rome_core::detect(&data).label());
        }
    }
    Ok(())
}
