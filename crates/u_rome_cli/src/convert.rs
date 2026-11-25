//! Convert decoded entries into common formats: PNG, glTF, Ogg/WAV, JSON.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;
use u_rome_core::AssetKind;

/// What to treat an entry as, when its detected kind is not enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum As {
    Auto,
    Texture,
    Model,
    Audio,
    Pso,
    Map,
    Raw,
}

fn target(kind: AssetKind, as_: As) -> As {
    if as_ != As::Auto {
        return as_;
    }
    match kind {
        AssetKind::TextureStore | AssetKind::TextureTable => As::Texture,
        AssetKind::Audio => As::Audio,
        AssetKind::Pso => As::Pso,
        AssetKind::MetaResource => As::Map,
        AssetKind::MeshBlob => As::Model,
        _ => As::Raw,
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Convert `bytes` into files under `out_dir` named after `stem`. Returns the
/// files written.
pub fn convert(
    bytes: &[u8],
    kind: AssetKind,
    as_: As,
    out_dir: &Path,
    stem: &str,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    match target(kind, as_) {
        As::Raw | As::Auto => Ok(vec![write(&out_dir.join(format!("{stem}.bin")), bytes)?]),
        As::Texture => textures(bytes, out_dir, stem),
        As::Audio => audio(bytes, out_dir, stem),
        As::Pso => pso(bytes, out_dir, stem),
        As::Map => map(bytes, out_dir, stem),
        As::Model => model(bytes, out_dir, stem),
    }
}

fn textures(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let store = u_rome_texture::TextureStore::parse(bytes).context("parsing texture store")?;
    let mut out = Vec::new();
    let mut failures = 0;
    for info in store.textures() {
        let name = format!("{stem}_{:03}_{:016x}.png", info.index, info.hash);
        match store
            .decode(bytes, info.index, u_rome_texture::MipSelection::PreferPlain)
            .and_then(|img| u_rome_texture::export_png(&img, 0))
        {
            Ok(png) => out.push(write(&out_dir.join(name), &png)?),
            Err(e) => {
                failures += 1;
                log::warn!(
                    "texture {} ({}x{} {:?}): {e}",
                    info.index,
                    info.width,
                    info.height,
                    info.format
                );
            }
        }
    }
    if out.is_empty() && failures > 0 {
        bail!("no texture in the store could be decoded ({failures} failed, see log)");
    }
    Ok(out)
}

fn audio(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let bank = u_rome_audio::AwcBank::parse(bytes.to_vec()).context("parsing AWC bank")?;
    let mut out = Vec::new();
    for (i, s) in bank.streams().iter().enumerate() {
        let base = format!("{stem}_{i:02}_{:08x}", s.id);
        if s.codec == u_rome_audio::Codec::Opus {
            match bank.ogg_opus(i) {
                Ok(ogg) => {
                    out.push(write(&out_dir.join(format!("{base}.ogg")), &ogg)?);
                    continue;
                }
                Err(e) => log::warn!("stream {i}: ogg export failed: {e}"),
            }
        }
        match u_rome_audio::decode_pcm(&bank, i) {
            Ok(pcm) => out.push(write(
                &out_dir.join(format!("{base}.wav")),
                &u_rome_audio::write_wav(&pcm),
            )?),
            Err(e) => log::warn!("stream {i} ({:?}): {e}", s.codec),
        }
    }
    Ok(out)
}

fn pso(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let doc = u_rome_pso::PsoDocument::parse(bytes).context("parsing PSIN document")?;
    let mut names = u_rome_pso::HashMapNames::new();
    for n in u_rome_pso::KNOWN_NAMES {
        names.add(n);
    }
    let root = doc.root().context("decoding root value")?;
    let value = u_rome_pso::to_json_with_schema(&root, &names, Some(doc.schema()));
    let wrapped = json!({
        "sections": doc.sections().iter().map(|s| json!({
            "tag": String::from_utf8_lossy(&s.tag),
            "offset": s.offset,
            "size": s.size,
        })).collect::<Vec<_>>(),
        "warnings": doc.warnings(),
        "root": value,
    });
    let text = serde_json::to_vec_pretty(&wrapped)?;
    Ok(vec![write(&out_dir.join(format!("{stem}.json")), &text)?])
}

fn map(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let meta = u_rome_map::MetaResource::parse(bytes).context("parsing meta resource")?;
    let map = u_rome_map::MapData::from_meta(&meta).context("decoding map data")?;
    let entities: Vec<_> = map
        .entities
        .iter()
        .map(|p| {
            json!({
                "archetype": format!("{}", p.archetype),
                "position": p.position,
                "rotation": p.rotation,
                "scale": [p.scale_xy, p.scale_z],
                "flags": p.flags,
                "lod_level": format!("{:?}", p.lod_level),
                "parent_index": p.parent_index,
                "num_children": p.num_children,
                "lod_dist": p.lod_dist,
                "guid": format!("{:016x}", p.guid),
            })
        })
        .collect();
    let doc = json!({
        "name": format!("{}", map.name),
        "parent": format!("{}", map.parent),
        "provisional": map.provisional,
        "warnings": map.warnings,
        "entities": entities,
        "prop_batches": map.prop_batches.len(),
        "grass_batches": map.grass_batches.len(),
    });
    let text = serde_json::to_vec_pretty(&doc)?;
    Ok(vec![write(&out_dir.join(format!("{stem}.json")), &text)?])
}

fn model(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let decoded = u_rome_model::decode_drawable(bytes).context("decoding drawable")?;
    for w in &decoded.warnings {
        log::warn!("model: {w}");
    }
    let mut out = Vec::new();
    for lod in 0..decoded.model.lods.len() {
        if decoded.model.lods[lod].meshes.is_empty() {
            continue;
        }
        let glb = u_rome_model::export_glb(&decoded.model, lod, &[])
            .with_context(|| format!("exporting LOD {lod}"))?;
        out.push(write(&out_dir.join(format!("{stem}_lod{lod}.glb")), &glb)?);
    }
    if out.is_empty() {
        bail!("drawable decoded but has no geometry");
    }
    Ok(out)
}
