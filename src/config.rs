use crate::one_or_many::OneOrMany;
use crate::pg::config::PgConfig;
use crate::source::IdResolver;
use crate::srv::config::SrvConfig;
use crate::srv::server::Sources;
use crate::utils;
use crate::utils::Error::{ConfigLoadError, ConfigParseError};
use crate::utils::Result;
use futures::future::try_join_all;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub srv: SrvConfig,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub postgres: Option<OneOrMany<PgConfig>>,

    #[serde(flatten)]
    pub unrecognized: HashMap<String, Value>,
}

impl Config {
    pub async fn resolve(&mut self, idr: IdResolver) -> Result<Sources> {
        if let Some(mut pg) = self.postgres.take() {
            Ok(try_join_all(pg.iter_mut().map(|s| s.resolve(idr.clone())))
                .await?
                .into_iter()
                .map(|s: (Sources, _)| s.0)
                .fold(HashMap::new(), |mut acc, hashmap| {
                    acc.extend(hashmap);
                    acc
                }))
        } else {
            Ok(HashMap::new())
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.unrecognized.extend(other.unrecognized);
        self.srv.merge(other.srv);

        if let Some(other) = other.postgres {
            match &mut self.postgres {
                Some(_first) => {
                    unimplemented!("merging multiple postgres configs is not yet supported");
                    // first.merge(other);
                }
                None => self.postgres = Some(other),
            }
        }
    }

    /// Apply defaults to the config, and validate if there is a connection string
    pub fn finalize(self) -> Result<Config> {
        report_unrecognized_config("", &self.unrecognized);
        Ok(Config {
            srv: self.srv,
            postgres: self
                .postgres
                .map(|pg| pg.map(|v| v.finalize().map_err(utils::Error::PostgresError)))
                .transpose()?,
            unrecognized: self.unrecognized,
        })
    }
}

/// Update empty option in place with a non-empty value from the second option.
pub fn set_option<T>(first: &mut Option<T>, second: Option<T>) {
    if first.is_none() && second.is_some() {
        *first = second;
    }
}

/// Merge two options
#[must_use]
pub fn merge_option<T>(
    first: Option<T>,
    second: Option<T>,
    merge: impl FnOnce(T, T) -> T,
) -> Option<T> {
    match (first, second) {
        (Some(first), Some(second)) => Some(merge(first, second)),
        (None, Some(second)) => Some(second),
        (first, None) => first,
    }
}

pub fn report_unrecognized_config(prefix: &str, unrecognized: &HashMap<String, Value>) {
    for key in unrecognized.keys() {
        warn!("Unrecognized config key: {prefix}{key}");
    }
}

/// Read config from a file
pub fn read_config(file_name: &Path) -> Result<Config> {
    let mut file = File::open(file_name).map_err(|e| ConfigLoadError(e, file_name.into()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| ConfigLoadError(e, file_name.into()))?;
    subst::yaml::from_str(contents.as_str(), &subst::Env)
        .map_err(|e| ConfigParseError(e, file_name.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg::utils::tests::{assert_config, some_str};
    use indoc::indoc;

    #[test]
    fn parse_config() {
        assert_config(
            indoc! {"
            ---
            keep_alive: 75
            listen_addresses: '0.0.0.0:3000'
            worker_processes: 8
        "},
            &Config {
                srv: SrvConfig {
                    keep_alive: Some(75),
                    listen_addresses: some_str("0.0.0.0:3000"),
                    worker_processes: Some(8),
                },
                ..Default::default()
            },
        );
    }
}
