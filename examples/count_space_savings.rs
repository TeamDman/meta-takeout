use eyre::Context;
use eyre::OptionExt;
use eyre::eyre;
use positioned_io::RandomAccessFile;
use rc_zip_tokio::ReadZip;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use thrumzip::get_zips::get_zips;
use thrumzip::path_inside_zip::PathInsideZip;
use thrumzip::path_to_zip::PathToZip;
use thrumzip::state::profiles::Profile;
use tracing::Level;

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.2} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.2} KB", b as f64 / KB as f64),
        _ => format!("{bytes} B"),
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install().wrap_err("Failed to install color_eyre")?;
    thrumzip::init_tracing::init_tracing(Level::INFO);
    // let profile = thrumzip::state::profiles::Profiles::load_and_get_active_profile().await?;
    let profile = Profile::new_example();

    // Collect zip files from both directories
    let (zip_paths, _) = get_zips(&profile.sources).await?;
    if zip_paths.is_empty() {
        eyre::bail!("No zip files found in {:?}", profile.sources);
    }
    println!("Found {} zip files", zip_paths.len());

    // build a map from each zip file to its set of entry paths
    let mut map: HashMap<PathToZip, HashSet<PathInsideZip>> = HashMap::new();
    // new: map from (zip_path, entry_path) to compressed_size
    let mut size_map: HashMap<(PathToZip, PathInsideZip), u64> = HashMap::new();
    for zip_path in &zip_paths {
        println!("Processing {zip_path:?}");
        let f = Arc::new(RandomAccessFile::open(zip_path)?);
        let archive = f.read_zip().await?;

        let mut names = HashSet::new();
        for entry in archive.entries() {
            let name: PathInsideZip = Arc::new(PathBuf::from(
                &entry
                    .sanitized_name()
                    .ok_or_eyre(eyre!("Entry had evil name: {:?}", entry.name))?,
            ))
            .into();
            // assert no duplicate within the same zip
            assert!(
                names.insert(name.clone()),
                "Duplicate entry {name:?} in archive {zip_path:?}"
            );
            // record compressed size
            size_map.insert((zip_path.clone(), name), entry.compressed_size);
        }
        map.insert(zip_path.clone(), names);
    }

    // for each pair of zip files, print how many common paths they share
    for i in 0..zip_paths.len() {
        for j in (i + 1)..zip_paths.len() {
            let p1 = &zip_paths[i];
            let p2 = &zip_paths[j];
            let set1 = &map[p1];
            let set2 = &map[p2];
            let common_paths: Vec<_> = set1.intersection(set2).collect();
            let common = common_paths.len();
            if common > 0 {
                let mut pair_bytes = 0u64;
                for name in &common_paths {
                    let s1 = size_map
                        .get(&(p1.clone(), (*name).clone()))
                        .copied()
                        .unwrap_or(0);
                    let s2 = size_map
                        .get(&(p2.clone(), (*name).clone()))
                        .copied()
                        .unwrap_or(0);
                    pair_bytes += s1.min(s2);
                }
                println!(
                    "{} and {} share {} paths, duplicated bytes: {} ({})",
                    p1.display(),
                    p2.display(),
                    common,
                    pair_bytes,
                    format_bytes(pair_bytes)
                );
            }
        }
    }

    // Build a map from entry name to all (zip_path, compressed_size) it appears in
    let mut entry_map: HashMap<PathInsideZip, Vec<(PathToZip, u64)>> = HashMap::new();
    for ((zip_path, entry_path), &size) in &size_map {
        entry_map
            .entry(entry_path.clone())
            .or_default()
            .push((zip_path.clone(), size));
    }

    // For each zip, calculate bytes present in other files
    let mut file_bytes: HashMap<&PathToZip, u64> = HashMap::new();
    let mut file_dup_bytes: HashMap<&PathToZip, u64> = HashMap::new();
    for zip_path in &zip_paths {
        let mut total = 0u64;
        let mut dup = 0u64;
        for entry in &map[zip_path] {
            let size = size_map
                .get(&(zip_path.clone(), entry.clone()))
                .copied()
                .unwrap_or(0);
            total += size;
            if let Some(zips) = entry_map.get(entry) {
                if zips.len() > 1 {
                    dup += size;
                }
            }
        }
        file_bytes.insert(zip_path, total);
        file_dup_bytes.insert(zip_path, dup);
    }

    // Print per-file duplicate stats
    for zip_path in &zip_paths {
        let total = file_bytes[zip_path];
        let dup = file_dup_bytes[zip_path];
        let percent = if total > 0 {
            (dup as f64) / (total as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "{}: {:.2}% ({}) of bytes present in other files",
            zip_path.display(),
            percent,
            format_bytes(dup)
        );
    }

    // Calculate total savable space (all-but-one for each entry)
    let mut total_savable = 0u64;
    let mut total_bytes = 0u64;
    for zips in entry_map.values() {
        if zips.len() > 1 {
            // Sort by size, keep one, sum the rest
            let mut sizes: Vec<u64> = zips.iter().map(|(_, s)| *s).collect();
            sizes.sort_unstable();
            // Save all but one
            for s in &sizes[..sizes.len() - 1] {
                total_savable += *s;
            }
        }
        // Count all bytes for total
        for (_, s) in zips {
            total_bytes += *s;
        }
    }
    let percent_reduction = if total_bytes > 0 {
        (total_savable as f64) / (total_bytes as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "Total deduplicatable bytes: {} ({:.2}% reduction)",
        format_bytes(total_savable),
        percent_reduction
    );

    println!("All entries processed successfully");
    Ok(())
}
