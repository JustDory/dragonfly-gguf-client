use anyhow::Result;
use clap::Parser;
use dragonfly_client_p2p as p2p;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "dfget seed-import",
    about = "Scan local HuggingFace model cache and register .gguf files as P2P seeds",
    long_about = "Walks ~/.cache/huggingface/hub (or --hf-cache) for .gguf files previously \
downloaded by the HuggingFace CLI or dfget, then writes a seed manifest for each one so a \
running dfdaemon can begin serving them to peers immediately.\n\n\
Examples:\n  \
# Import everything in the default HF cache (dry-run first):\n  \
dfget seed-import --dry-run\n\n  \
# Actually register all found files:\n  \
dfget seed-import\n\n  \
# Use a custom cache directory:\n  \
dfget seed-import --hf-cache /mnt/fast/hf-cache"
)]
struct Args {
    #[arg(
        long,
        value_name = "PATH",
        help = "HuggingFace cache directory to scan \
(default: $HF_HOME/hub, $XDG_CACHE_HOME/huggingface/hub, or ~/.cache/huggingface/hub)"
    )]
    hf_cache: Option<std::path::PathBuf>,

    #[arg(
        long,
        default_value = "https://tracker.dragonfly-gguf.dev",
        env = "DRAGONFLY_P2P_TRACKER",
        help = "Tracker URL to register seeds with"
    )]
    p2p_tracker: String,

    #[arg(
        long,
        default_value_t = 0,
        value_name = "SECS",
        env = "DRAGONFLY_SEED_TIME",
        help = "How long to advertise each seed in seconds (0 = seed forever)"
    )]
    seed_time: u64,

    #[arg(
        long,
        default_value_t = false,
        help = "Print what would be registered without writing any manifests"
    )]
    dry_run: bool,
}

/// Entry point called by dfget main when the first arg is "seed-import".
pub async fn run(remaining_args: &[String]) -> Result<()> {
    // Re-attach the binary name so clap's error messages look right.
    let argv =
        std::iter::once("dfget seed-import".to_string()).chain(remaining_args.iter().cloned());
    let args = Args::try_parse_from(argv).unwrap_or_else(|e| e.exit());

    let hf_cache = args.hf_cache.unwrap_or_else(p2p::default_hf_cache_dir);

    println!("Scanning {} …", hf_cache.display());
    let candidates = p2p::scan_hf_cache(&hf_cache);

    if candidates.is_empty() {
        println!("No .gguf files found in {}", hf_cache.display());
        return Ok(());
    }

    println!("Found {} .gguf file(s):\n", candidates.len());

    let registry_dir = p2p::default_registry_dir();
    let seed_duration = Duration::from_secs(args.seed_time);
    let mut registered = 0usize;
    let mut failed = 0usize;

    for candidate in &candidates {
        let key = p2p::content_key(&candidate.gguf_url, &candidate.revision, None);
        let short_key = &key[..8];

        if args.dry_run {
            println!(
                "  [dry-run] {}\n    rev:  {}\n    path: {}\n    key:  {short_key}…\n",
                candidate.gguf_url,
                candidate.revision,
                candidate.file_path.display(),
            );
            registered += 1;
            continue;
        }

        match p2p::register_seed(
            &registry_dir,
            &args.p2p_tracker,
            &key,
            &candidate.file_path,
            seed_duration,
        ) {
            Ok(()) => {
                println!("  ✓ {} ({short_key}…)", candidate.gguf_url);
                registered += 1;
            }
            Err(e) => {
                eprintln!("  ✗ {}: {e}", candidate.gguf_url);
                failed += 1;
            }
        }
    }

    println!();
    if args.dry_run {
        println!("{registered} file(s) would be registered (--dry-run, nothing written)");
    } else {
        let fail_note = if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        };
        println!("{registered} seed(s) registered{fail_note}");

        if registered > 0 {
            println!(
                "\nManifests written to {}",
                registry_dir.display()
            );
            println!(
                "A running dfdaemon will pick them up within ~30 seconds and begin announcing."
            );
        }
    }

    Ok(())
}
