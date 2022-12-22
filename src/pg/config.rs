use crate::config::{report_unrecognized_config, set_option};
use crate::pg::config_function::FuncInfoSources;
use crate::pg::config_table::TableInfoSources;
use crate::pg::configurator::PgBuilder;
use crate::pg::pool::Pool;
use crate::pg::utils::PgError::NoConnectionString;
use crate::pg::utils::Result;
use crate::source::IdResolver;
use crate::srv::server::Sources;
use crate::utils::Schemas;
use futures::future::try_join;
use serde::{Deserialize, Serialize};
use tilejson::TileJSON;

pub trait PgInfo {
    fn format_id(&self) -> String;
    fn to_tilejson(&self) -> TileJSON;
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PgConfig {
    pub connection_string: Option<String>,
    #[cfg(feature = "ssl")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_root_file: Option<std::path::PathBuf>,
    #[cfg(feature = "ssl")]
    #[serde(default, skip_serializing_if = "Clone::clone")]
    pub danger_accept_invalid_certs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_srid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_size: Option<u32>,
    #[serde(skip)]
    pub auto_tables: Option<Schemas>,
    #[serde(skip)]
    pub auto_functions: Option<Schemas>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<TableInfoSources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<FuncInfoSources>,
    #[serde(skip)]
    pub run_autodiscovery: bool,
}

impl PgConfig {
    pub fn merge(&mut self, other: Self) -> &mut Self {
        set_option(&mut self.connection_string, other.connection_string);
        #[cfg(feature = "ssl")]
        {
            set_option(&mut self.ca_root_file, other.ca_root_file);
            self.danger_accept_invalid_certs |= other.danger_accept_invalid_certs;
        }
        set_option(&mut self.default_srid, other.default_srid);
        set_option(&mut self.pool_size, other.pool_size);
        set_option(&mut self.auto_tables, other.auto_tables);
        set_option(&mut self.auto_functions, other.auto_functions);
        set_option(&mut self.tables, other.tables);
        set_option(&mut self.functions, other.functions);
        self
    }

    /// Apply defaults to the config, and validate if there is a connection string
    pub fn finalize(self) -> Result<PgConfig> {
        if let Some(ref ts) = self.tables {
            for (k, v) in ts {
                report_unrecognized_config(&format!("tables.{k}."), &v.unrecognized);
            }
        }
        if let Some(ref fs) = self.functions {
            for (k, v) in fs {
                report_unrecognized_config(&format!("functions.{k}."), &v.unrecognized);
            }
        }
        let connection_string = self.connection_string.ok_or(NoConnectionString)?;

        Ok(PgConfig {
            connection_string: Some(connection_string),
            run_autodiscovery: self.tables.is_none() && self.functions.is_none(),
            ..self
        })
    }

    pub async fn resolve(&mut self, id_resolver: IdResolver) -> Result<(Sources, Pool)> {
        let pg = PgBuilder::new(self, id_resolver).await?;
        let ((mut tables, tbl_info), (funcs, func_info)) =
            try_join(pg.instantiate_tables(), pg.instantiate_functions()).await?;

        self.tables = Some(tbl_info);
        self.functions = Some(func_info);
        tables.extend(funcs);
        Ok((tables, pg.get_pool()))
    }

    #[must_use]
    pub fn is_autodetect(&self) -> bool {
        self.run_autodiscovery
    }
}

#[must_use]
pub fn is_postgresql_string(s: &str) -> bool {
    s.starts_with("postgresql://") || s.starts_with("postgres://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::one_or_many::OneOrMany::{Many, One};
    use crate::pg::config_function::FunctionInfo;
    use crate::pg::config_table::TableInfo;
    use crate::pg::utils::tests::{assert_config, some_str};
    use indoc::indoc;
    use std::collections::HashMap;
    use tilejson::Bounds;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn parse_config() {
        assert_config(
            indoc! {"
            ---
            postgres:
              connection_string: 'postgresql://postgres@localhost/db'
        "},
            &Config {
                postgres: Some(One(PgConfig {
                    connection_string: some_str("postgresql://postgres@localhost/db"),
                    run_autodiscovery: true,
                    ..Default::default()
                })),
                ..Default::default()
            },
        );

        assert_config(
            indoc! {"
            ---
            postgres:
              - connection_string: 'postgres://postgres@localhost:5432/db'
              - connection_string: 'postgresql://postgres@localhost:5433/db'
        "},
            &Config {
                postgres: Some(Many(vec![
                    PgConfig {
                        connection_string: some_str("postgres://postgres@localhost:5432/db"),
                        run_autodiscovery: true,
                        ..Default::default()
                    },
                    PgConfig {
                        connection_string: some_str("postgresql://postgres@localhost:5433/db"),
                        run_autodiscovery: true,
                        ..Default::default()
                    },
                ])),
                ..Default::default()
            },
        );

        assert_config(
            indoc! {"
            ---
            postgres:
              connection_string: 'postgres://postgres@localhost:5432/db'
              default_srid: 4326
              pool_size: 20
            
              tables:
                table_source:
                  schema: public
                  table: table_source
                  srid: 4326
                  geometry_column: geom
                  id_column: ~
                  minzoom: 0
                  maxzoom: 30
                  bounds: [-180.0, -90.0, 180.0, 90.0]
                  extent: 4096
                  buffer: 64
                  clip_geom: true
                  geometry_type: GEOMETRY
                  properties:
                    gid: int4
            
              functions:
                function_zxy_query:
                  schema: public
                  function: function_zxy_query
                  minzoom: 0
                  maxzoom: 30
                  bounds: [-180.0, -90.0, 180.0, 90.0]
        "},
            &Config {
                postgres: Some(One(PgConfig {
                    connection_string: some_str("postgres://postgres@localhost:5432/db"),
                    default_srid: Some(4326),
                    pool_size: Some(20),
                    tables: Some(HashMap::from([(
                        "table_source".to_string(),
                        TableInfo {
                            schema: "public".to_string(),
                            table: "table_source".to_string(),
                            srid: 4326,
                            geometry_column: "geom".to_string(),
                            minzoom: Some(0),
                            maxzoom: Some(30),
                            bounds: Some([-180, -90, 180, 90].into()),
                            extent: Some(4096),
                            buffer: Some(64),
                            clip_geom: Some(true),
                            geometry_type: some_str("GEOMETRY"),
                            properties: HashMap::from([("gid".to_string(), "int4".to_string())]),
                            ..Default::default()
                        },
                    )])),
                    functions: Some(HashMap::from([(
                        "function_zxy_query".to_string(),
                        FunctionInfo::new_extended(
                            "public".to_string(),
                            "function_zxy_query".to_string(),
                            0,
                            30,
                            Bounds::MAX,
                        ),
                    )])),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
    }
}
