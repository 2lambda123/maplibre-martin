#![allow(clippy::missing_errors_doc)]

extern crate core;

use std::ffi::OsStr;
use std::fmt::Display;
use std::path::Path;
use std::str::FromStr;

use futures::TryStreamExt;
use log::{debug, info, warn};
use martin_tile_utils::{Format, TileInfo};
use serde_json::{Value as JSONValue, Value};
use sqlx::{query, SqliteExecutor};
use tilejson::{tilejson, Bounds, Center, TileJSON};

use crate::errors::{MbtError, MbtResult};
use crate::mbtiles_queries::{is_deduplicated_type, is_tile_tables_type};

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub id: String,
    pub tile_info: TileInfo,
    pub layer_type: Option<String>,
    pub tilejson: TileJSON,
    pub json: Option<JSONValue>,
}

#[derive(Debug, PartialEq)]
pub enum MbtType {
    TileTables,
    DeDuplicated,
}

#[derive(Clone, Debug)]
pub struct Mbtiles {
    filepath: String,
    filename: String,
}

impl Mbtiles {
    pub fn new<P: AsRef<Path>>(filepath: P) -> MbtResult<Self> {
        let path = filepath.as_ref();
        Ok(Self {
            filepath: path
                .to_str()
                .ok_or_else(|| MbtError::UnsupportedCharsInFilepath(path.to_path_buf()))?
                .to_string(),
            filename: path
                .file_stem()
                .unwrap_or_else(|| OsStr::new("unknown"))
                .to_string_lossy()
                .to_string(),
        })
    }

    pub fn filepath(&self) -> &str {
        &self.filepath
    }

    pub fn filename(&self) -> &str {
        &self.filename
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

    pub async fn get_metadata_value<T>(&self, conn: &mut T, key: &str) -> MbtResult<Option<String>>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
        let query = query!("SELECT value from metadata where name = ?", key);
        let row = query.fetch_optional(conn).await?;
        if let Some(row) = row {
            if let Some(value) = row.value {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    pub async fn get_metadata<T>(&self, conn: &mut T) -> MbtResult<Metadata>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
        let (tj, layer_type, json) = self.parse_metadata(conn).await?;

        Ok(Metadata {
            id: self.filename.to_string(),
            tile_info: self.detect_format(&tj, conn).await?,
            tilejson: tj,
            layer_type,
            json,
        })
    }

    async fn parse_metadata<T>(
        &self,
        conn: &mut T,
    ) -> MbtResult<(TileJSON, Option<String>, Option<Value>)>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
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

    async fn detect_format<T>(&self, tilejson: &TileJSON, conn: &mut T) -> MbtResult<TileInfo>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
        let mut tile_info = None;
        let mut tested_zoom = -1_i64;

        // First, pick any random tile
        let query = query!("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles WHERE zoom_level >= 0 LIMIT 1");
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

    pub async fn get_tile<T>(
        &self,
        conn: &mut T,
        z: u8,
        x: u32,
        y: u32,
    ) -> MbtResult<Option<Vec<u8>>>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
        // let mut conn = self.pool.acquire().await?;
        let y = (1 << z) - 1 - y;
        let query = query! {"SELECT tile_data from tiles where zoom_level = ? AND tile_column = ? AND tile_row = ?", z, x, y};
        let row = query.fetch_optional(conn).await?;
        if let Some(row) = row {
            if let Some(tile_data) = row.tile_data {
                return Ok(Some(tile_data));
            }
        }
        Ok(None)
    }

    pub async fn detect_type<T>(&self, conn: &mut T) -> MbtResult<MbtType>
    where
        for<'e> &'e mut T: SqliteExecutor<'e>,
    {
        if is_deduplicated_type(&mut *conn).await? {
            Ok(MbtType::DeDuplicated)
        } else if is_tile_tables_type(&mut *conn).await? {
            Ok(MbtType::TileTables)
        } else {
            Err(MbtError::InvalidDataFormat(self.filepath.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use martin_tile_utils::Encoding;
    use sqlx::{Connection, SqliteConnection};
    use tilejson::VectorLayer;

    use super::*;

    async fn open(filepath: &str) -> (SqliteConnection, Mbtiles) {
        let mbt = Mbtiles::new(filepath).unwrap();
        (
            SqliteConnection::connect(mbt.filepath()).await.unwrap(),
            mbt,
        )
    }

    #[actix_rt::test]
    async fn mbtiles_meta() {
        let filepath = "../tests/fixtures/files/geography-class-jpg.mbtiles";
        let mbt = Mbtiles::new(filepath).unwrap();
        assert_eq!(mbt.filepath(), filepath);
        assert_eq!(mbt.filename(), "geography-class-jpg");
    }

    #[actix_rt::test]
    async fn metadata_jpeg() {
        let (mut conn, mbt) = open("../tests/fixtures/files/geography-class-jpg.mbtiles").await;
        let metadata = mbt.get_metadata(&mut conn).await.unwrap();
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
        let (mut conn, mbt) = open("../tests/fixtures/files/world_cities.mbtiles").await;
        let metadata = mbt.get_metadata(&mut conn).await.unwrap();
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

    #[actix_rt::test]
    async fn metadata_get_key() {
        let (mut conn, mbt) = open("../tests/fixtures/files/world_cities.mbtiles").await;

        let res = mbt.get_metadata_value(&mut conn, "bounds").await.unwrap();
        assert_eq!(res.unwrap(), "-123.123590,-37.818085,174.763027,59.352706");
        let res = mbt.get_metadata_value(&mut conn, "name").await.unwrap();
        assert_eq!(res.unwrap(), "Major cities from Natural Earth data");
        let res = mbt.get_metadata_value(&mut conn, "maxzoom").await.unwrap();
        assert_eq!(res.unwrap(), "6");
        let res = mbt.get_metadata_value(&mut conn, "nonexistent_key").await;
        assert_eq!(res.unwrap(), None);
        let res = mbt.get_metadata_value(&mut conn, "").await;
        assert_eq!(res.unwrap(), None);
    }

    #[actix_rt::test]
    async fn detect_type() {
        let (mut conn, mbt) = open("../tests/fixtures/files/world_cities.mbtiles").await;
        let res = mbt.detect_type(&mut conn).await.unwrap();
        assert_eq!(res, MbtType::TileTables);

        let (mut conn, mbt) = open("../tests/fixtures/files/geography-class-jpg.mbtiles").await;
        let res = mbt.detect_type(&mut conn).await.unwrap();
        assert_eq!(res, MbtType::DeDuplicated);

        let (mut conn, mbt) = open(":memory:").await;
        let res = mbt.detect_type(&mut conn).await;
        assert!(matches!(res, Err(MbtError::InvalidDataFormat(_))));
    }
}
