#![allow(clippy::missing_errors_doc)]

extern crate core;

use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use futures::TryStreamExt;
use log::{debug, info, warn};
use martin_tile_utils::{Format, TileInfo};
use serde_json::{Value as JSONValue, Value};
use sqlx::pool::PoolConnection;
use sqlx::sqlite::SqlitePool;
use sqlx::{query, Pool, Sqlite};
use tilejson::{tilejson, Bounds, Center, TileJSON};

#[derive(thiserror::Error, Debug)]
pub enum MbtError {
    #[error("SQL Error {0}")]
    SqlError(#[from] sqlx::Error),

    #[error("MBTile filepath contains unsupported characters: {}", .0.display())]
    UnsupportedCharsInFilepath(PathBuf),

    #[error("Inconsistent tile formats detected: {0} vs {1}")]
    InconsistentMetadata(TileInfo, TileInfo),

    #[error("No tiles found")]
    NoTilesFound,
}

type MbtResult<T> = Result<T, MbtError>;

#[derive(Clone, Debug)]
pub struct Mbtiles {
    filename: String,
    pool: Pool<Sqlite>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub id: String,
    pub tile_info: TileInfo,
    pub layer_type: Option<String>,
    pub tilejson: TileJSON,
    pub json: Option<JSONValue>,
}

impl Mbtiles {
    pub async fn new<P: AsRef<Path>>(filepath: P) -> MbtResult<Self> {
        let file = filepath
            .as_ref()
            .to_str()
            .ok_or_else(|| MbtError::UnsupportedCharsInFilepath(filepath.as_ref().to_path_buf()))?;
        let pool = SqlitePool::connect(file).await?;
        let filename = filepath
            .as_ref()
            .file_stem()
            .unwrap_or_else(|| OsStr::new("unknown"))
            .to_string_lossy()
            .to_string();
        Ok(Self { filename, pool })
    }

    fn to_val<V, E: Display>(&self, val: Result<V, E>, title: &str) -> Option<V> {
        match val {
            Ok(v) => Some(v),
            Err(err) => {
                let name = &self.filename;
                warn!("Unable to parse metadata {title} value in {name}: {err}");
                None
            }
        }
    }

    pub async fn get_metadata(&self) -> MbtResult<Metadata> {
        let mut conn = self.pool.acquire().await?;

        let (tj, layer_type, json) = self.parse_metadata(&mut conn).await?;

        Ok(Metadata {
            id: self.filename.to_string(),
            tile_info: self.detect_format(&tj, &mut conn).await?,
            tilejson: tj,
            layer_type,
            json,
        })
    }

    async fn parse_metadata(
        &self,
        conn: &mut PoolConnection<Sqlite>,
    ) -> MbtResult<(TileJSON, Option<String>, Option<Value>)> {
        let query = query!("SELECT name, value FROM metadata WHERE value IS NOT ''");
        let mut rows = query.fetch(conn);

        let mut tj = tilejson! { tiles: vec![] };
        let mut layer_type: Option<String> = None;
        let mut json: Option<JSONValue> = None;

        while let Some(row) = rows.try_next().await? {
            if let (Some(name), Some(value)) = (row.name, row.value) {
                match name.as_ref() {
                    "name" => tj.name = Some(value),
                    "version" => tj.version = Some(value),
                    "bounds" => tj.bounds = self.to_val(Bounds::from_str(value.as_str()), &name),
                    "center" => tj.center = self.to_val(Center::from_str(value.as_str()), &name),
                    "minzoom" => tj.minzoom = self.to_val(value.parse(), &name),
                    "maxzoom" => tj.maxzoom = self.to_val(value.parse(), &name),
                    "description" => tj.description = Some(value),
                    "attribution" => tj.attribution = Some(value),
                    "type" => layer_type = Some(value),
                    "legend" => tj.legend = Some(value),
                    "template" => tj.template = Some(value),
                    "json" => json = self.to_val(serde_json::from_str(&value), &name),
                    "format" | "generator" => {
                        tj.other.insert(name, Value::String(value));
                    }
                    _ => {
                        let file = &self.filename;
                        warn!("{file} has an unrecognized metadata value {name}={value}");
                        tj.other.insert(name, Value::String(value));
                    }
                }
            }
        }

        if let Some(JSONValue::Object(obj)) = &mut json {
            if let Some(value) = obj.remove("vector_layers") {
                if let Ok(v) = serde_json::from_value(value) {
                    tj.vector_layers = Some(v);
                } else {
                    warn!(
                        "Unable to parse metadata vector_layers value in {}",
                        self.filename
                    );
                }
            }
        }

        Ok((tj, layer_type, json))
    }

    async fn detect_format(
        &self,
        tilejson: &TileJSON,
        conn: &mut PoolConnection<Sqlite>,
    ) -> MbtResult<TileInfo> {
        let mut tile_info = None;
        let mut tested_zoom = -1_i64;

        // First, pick any random tile
        let query = query! {"SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles WHERE zoom_level >= 0 LIMIT 1"};
        let row = query.fetch_optional(&mut *conn).await?;
        if let Some(r) = row {
            tile_info = self.parse_tile(r.zoom_level, r.tile_column, r.tile_row, r.tile_data);
            tested_zoom = r.zoom_level.unwrap_or(-1);
        }

        // Afterwards, iterate over tiles in all allowed zooms and check for consistency
        for z in tilejson.minzoom.unwrap_or(0)..=tilejson.maxzoom.unwrap_or(18) {
            if i64::from(z) == tested_zoom {
                continue;
            }
            let query = query! {"SELECT tile_column, tile_row, tile_data FROM tiles WHERE zoom_level = ? LIMIT 1", z};
            let row = query.fetch_optional(&mut *conn).await?;
            if let Some(r) = row {
                match (
                    tile_info,
                    self.parse_tile(Some(z.into()), r.tile_column, r.tile_row, r.tile_data),
                ) {
                    (_, None) => {}
                    (None, new) => tile_info = new,
                    (Some(old), Some(new)) if old == new => {}
                    (Some(old), Some(new)) => {
                        return Err(MbtError::InconsistentMetadata(old, new));
                    }
                }
            }
        }

        if let Some(Value::String(fmt)) = tilejson.other.get("format") {
            let file = &self.filename;
            match (tile_info, Format::parse(fmt)) {
                (_, None) => {
                    warn!("Unknown format value in metadata: {fmt}");
                }
                (None, Some(fmt)) => {
                    if fmt.is_detectable() {
                        warn!("Metadata table sets detectable '{fmt}' tile format, but it could not be verified for file {file}");
                    } else {
                        info!("Using '{fmt}' tile format from metadata table in file {file}");
                    }
                    tile_info = Some(fmt.into());
                }
                (Some(info), Some(fmt)) if info.format == fmt => {
                    debug!("Detected tile format {info} matches metadata.format '{fmt}' in file {file}");
                }
                (Some(info), _) => {
                    warn!("Found inconsistency: metadata.format='{fmt}', but tiles were detected as {info:?} in file {file}. Tiles will be returned as {info:?}.");
                }
            }
        }

        if let Some(info) = tile_info {
            if info.format != Format::Mvt && tilejson.vector_layers.is_some() {
                warn!(
                    "{} has vector_layers metadata but non-vector tiles",
                    self.filename
                );
            }
            Ok(info)
        } else {
            Err(MbtError::NoTilesFound)
        }
    }

    fn parse_tile(
        &self,
        z: Option<i64>,
        x: Option<i64>,
        y: Option<i64>,
        tile: Option<Vec<u8>>,
    ) -> Option<TileInfo> {
        if let (Some(z), Some(x), Some(y), Some(tile)) = (z, x, y, tile) {
            let info = TileInfo::detect(&tile);
            if let Some(info) = info {
                debug!(
                    "Tile {z}/{x}/{} is detected as {info} in file {}",
                    (1 << z) - 1 - y,
                    self.filename,
                );
            }
            info
        } else {
            None
        }
    }

    pub async fn get_tile(&self, z: u8, x: u32, y: u32) -> MbtResult<Option<Vec<u8>>> {
        let mut conn = self.pool.acquire().await?;
        let y = (1 << z) - 1 - y;
        let query = query! {"SELECT tile_data from tiles where zoom_level = ? AND tile_column = ? AND tile_row = ?", z, x, y};
        let row = query.fetch_optional(&mut conn).await?;
        if let Some(row) = row {
            if let Some(tile_data) = row.tile_data {
                return Ok(Some(tile_data));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use martin_tile_utils::Encoding;
    use tilejson::VectorLayer;

    use super::*;

    #[actix_rt::test]
    async fn metadata_jpeg() {
        let mbt = Mbtiles::new(Path::new(
            "../tests/fixtures/files/geography-class-jpg.mbtiles",
        ))
        .await;
        let mbt = mbt.unwrap();
        let metadata = mbt.get_metadata().await.unwrap();
        let tj = metadata.tilejson;

        assert_eq!(tj.description.unwrap(), "One of the example maps that comes with TileMill - a bright & colorful world map that blends retro and high-tech with its folded paper texture and interactive flag tooltips. ");
        assert!(tj.legend.unwrap().starts_with("<div style="));
        assert_eq!(tj.maxzoom.unwrap(), 1);
        assert_eq!(tj.minzoom.unwrap(), 0);
        assert_eq!(tj.name.unwrap(), "Geography Class");
        assert_eq!(tj.template.unwrap(),"{{#__location__}}{{/__location__}}{{#__teaser__}}<div style=\"text-align:center;\">\n\n<img src=\"data:image/png;base64,{{flag_png}}\" style=\"-moz-box-shadow:0px 1px 3px #222;-webkit-box-shadow:0px 1px 5px #222;box-shadow:0px 1px 3px #222;\"><br>\n<strong>{{admin}}</strong>\n\n</div>{{/__teaser__}}{{#__full__}}{{/__full__}}");
        assert_eq!(tj.version.unwrap(), "1.0.0");
        assert_eq!(metadata.id, "geography-class-jpg");
        assert_eq!(metadata.tile_info, Format::Jpeg.into());
    }

    #[actix_rt::test]
    async fn metadata_mvt() {
        let mbt = Mbtiles::new(Path::new("../tests/fixtures/files/world_cities.mbtiles")).await;
        let mbt = mbt.unwrap();
        let metadata = mbt.get_metadata().await.unwrap();
        let tj = metadata.tilejson;

        assert_eq!(tj.maxzoom.unwrap(), 6);
        assert_eq!(tj.minzoom.unwrap(), 0);
        assert_eq!(tj.name.unwrap(), "Major cities from Natural Earth data");
        assert_eq!(tj.version.unwrap(), "2");
        assert_eq!(
            tj.vector_layers,
            Some(vec![VectorLayer {
                id: "cities".to_string(),
                fields: vec![("name".to_string(), "String".to_string())]
                    .into_iter()
                    .collect(),
                description: Some(String::new()),
                minzoom: Some(0),
                maxzoom: Some(6),
                other: HashMap::default()
            }])
        );
        assert_eq!(metadata.id, "world_cities");
        assert_eq!(
            metadata.tile_info,
            TileInfo::new(Format::Mvt, Encoding::Gzip)
        );
        assert_eq!(metadata.layer_type, Some("overlay".to_string()));
    }
}
