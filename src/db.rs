use std::io;
use std::str::FromStr;

use log::{error, info};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use r2d2::PooledConnection;
use r2d2_postgres::PostgresConnectionManager;
use semver::Version;
use semver::VersionReq;

use crate::utils::prettify_error;

pub type ConnectionManager = PostgresConnectionManager<MakeTlsConnector>;
pub type Pool = r2d2::Pool<ConnectionManager>;
pub type Connection = PooledConnection<ConnectionManager>;

fn make_tls_connector(
    ca_root_file: &Option<String>,
    danger_accept_invalid_certs: bool,
) -> io::Result<MakeTlsConnector> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;

    if danger_accept_invalid_certs {
        builder.set_verify(SslVerifyMode::NONE);
    }

    if let Some(ca_root_file) = ca_root_file {
        info!("Using {ca_root_file} as trusted root certificate");
        builder.set_ca_file(ca_root_file)?;
    }

    let tls_connector = MakeTlsConnector::new(builder.build());
    Ok(tls_connector)
}

pub fn setup_connection_pool(
    connection_string: &str,
    ca_root_file: &Option<String>,
    pool_size: Option<u32>,
    danger_accept_invalid_certs: bool,
) -> io::Result<Pool> {
    let config = postgres::config::Config::from_str(connection_string)
        .map_err(|e| prettify_error!(e, "Can't parse connection string"))?;

    let tls_connector = make_tls_connector(ca_root_file, danger_accept_invalid_certs)
        .map_err(|e| prettify_error!(e, "Can't build TLS connection"))?;

    let manager = PostgresConnectionManager::new(config, tls_connector);

    let pool = r2d2::Pool::builder()
        .max_size(pool_size.unwrap_or(20))
        .build(manager)
        .map_err(|e| prettify_error!(e, "Can't build connection pool"))?;

    Ok(pool)
}

pub fn get_connection(pool: &Pool) -> io::Result<Connection> {
    let connection = pool
        .get()
        .map_err(|e| prettify_error!(e, "Can't retrieve connection from the pool"))?;

    Ok(connection)
}

pub fn select_postgis_verion(pool: &Pool) -> io::Result<String> {
    let mut connection = get_connection(pool)?;

    let version = connection
        .query_one(include_str!("scripts/get_postgis_version.sql"), &[])
        .map(|row| row.get::<_, String>("postgis_version"))
        .map_err(|e| prettify_error!(e, "Can't get PostGIS version"))?;

    Ok(version)
}

pub fn check_postgis_version(required_postgis_version: &str, pool: &Pool) -> io::Result<bool> {
    let postgis_version = select_postgis_verion(pool)?;

    let req = VersionReq::parse(required_postgis_version)
        .map_err(|e| prettify_error!(e, "Can't parse required PostGIS version"))?;

    let version = Version::parse(postgis_version.as_str())
        .map_err(|e| prettify_error!(e, "Can't parse database PostGIS version"))?;

    let matches = req.matches(&version);

    if !matches {
        error!("Martin requires PostGIS {required_postgis_version}, current version is {postgis_version}");
    }

    Ok(matches)
}
