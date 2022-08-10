use std::{env, io};

use actix_web::dev::Server;
use docopt::Docopt;
use log::{error, info, warn};
use martin::config::{read_config, Config, ConfigBuilder};
use martin::db::{check_postgis_version, get_connection, setup_connection_pool, Pool};
use martin::function_source::get_function_sources;
use martin::table_source::get_table_sources;
use martin::{prettify_error, server};
use serde::Deserialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REQUIRED_POSTGIS_VERSION: &str = ">= 2.4.0";

pub const USAGE: &str = "
Martin - PostGIS Mapbox Vector Tiles server.

Usage:
  martin [options] [<connection>]
  martin -h | --help
  martin -v | --version

Options:
  -h --help                         Show this screen.
  -v --version                      Show version.
  --config=<path>                   Path to config file.
  --keep-alive=<n>                  Connection keep alive timeout [default: 75].
  --listen-addresses=<n>            The socket address to bind [default: 0.0.0.0:3000].
  --default-srid=<n>                If a spatial table has SRID 0, then this default SRID will be used as a fallback.
  --pool-size=<n>                   Maximum connections pool size [default: 20].
  --workers=<n>                     Number of web server workers.
  --ca-root-file=<path>             Loads trusted root certificates from a file. The file should contain a sequence of PEM-formatted CA certificates.
  --danger-accept-invalid-certs     Trust invalid certificates. This introduces significant vulnerabilities, and should only be used as a last resort.
  --watch                           [IGNORED] This flag is no longer supported, and will be ignored.
";

#[derive(Debug, Deserialize)]
pub struct Args {
    pub arg_connection: Option<String>,
    pub flag_config: Option<String>,
    pub flag_help: bool,
    pub flag_keep_alive: Option<usize>,
    pub flag_listen_addresses: Option<String>,
    pub flag_pool_size: Option<u32>,
    pub flag_watch: bool,
    pub flag_version: bool,
    pub flag_workers: Option<usize>,
    pub flag_default_srid: Option<i32>,
    pub flag_ca_root_file: Option<String>,
    pub flag_danger_accept_invalid_certs: bool,
}

pub async fn generate_config(args: Args, pool: &Pool) -> io::Result<Config> {
    let connection_string = args.arg_connection.clone().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "Database connection string is not set",
        )
    })?;

    let mut connection = get_connection(pool).await?;
    let table_sources = get_table_sources(&mut connection, &args.flag_default_srid).await?;
    let function_sources = get_function_sources(&mut connection).await?;

    let config = ConfigBuilder {
        connection_string,
        keep_alive: args.flag_keep_alive,
        listen_addresses: args.flag_listen_addresses,
        default_srid: args.flag_default_srid,
        pool_size: args.flag_pool_size,
        worker_processes: args.flag_workers,
        table_sources: Some(table_sources),
        function_sources: Some(function_sources),
        ca_root_file: None,
        danger_accept_invalid_certs: Some(args.flag_danger_accept_invalid_certs),
    };

    let config = config.finalize();
    Ok(config)
}

async fn setup_from_config(file_name: String) -> io::Result<(Config, Pool)> {
    let config = read_config(&file_name).map_err(|e| prettify_error!(e, "Can't read config"))?;

    let pool = setup_connection_pool(
        &config.connection_string,
        &config.ca_root_file,
        Some(config.pool_size),
        config.danger_accept_invalid_certs,
    )
    .await
    .map_err(|e| prettify_error!(e, "Can't setup connection pool"))?;

    if let Some(table_sources) = &config.table_sources {
        for table_source in table_sources.values() {
            info!(
                r#"Found "{}" table source with "{}" column ({}, SRID={})"#,
                table_source.id,
                table_source.geometry_column,
                table_source
                    .geometry_type
                    .as_ref()
                    .unwrap_or(&"null".to_string()),
                table_source.srid
            );
        }
    }

    if let Some(function_sources) = &config.function_sources {
        for function_source in function_sources.values() {
            info!("Found {} function source", function_source.id);
        }
    }

    info!("Connected to {}", config.connection_string);

    Ok((config, pool))
}

async fn setup_from_args(args: Args) -> io::Result<(Config, Pool)> {
    let connection_string = args.arg_connection.clone().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "Database connection string is not set",
        )
    })?;

    info!("Connecting to database");
    let pool = setup_connection_pool(
        &connection_string,
        &args.flag_ca_root_file,
        args.flag_pool_size,
        args.flag_danger_accept_invalid_certs,
    )
    .await
    .map_err(|e| prettify_error!(e, "Can't setup connection pool"))?;

    info!("Scanning database");
    let config = generate_config(args, &pool)
        .await
        .map_err(|e| prettify_error!(e, "Can't generate config"))?;

    Ok((config, pool))
}

fn parse_env(args: Args) -> Args {
    let arg_connection = args.arg_connection.or_else(|| {
        env::var_os("DATABASE_URL").and_then(|connection| connection.into_string().ok())
    });

    let flag_default_srid = args.flag_default_srid.or_else(|| {
        env::var_os("DEFAULT_SRID").and_then(|srid| {
            srid.into_string()
                .ok()
                .and_then(|srid| srid.parse::<i32>().ok())
        })
    });

    let flag_ca_root_file = args.flag_ca_root_file.or_else(|| {
        env::var_os("CA_ROOT_FILE").and_then(|connection| connection.into_string().ok())
    });

    let flag_danger_accept_invalid_certs = args.flag_danger_accept_invalid_certs
        || env::var_os("DANGER_ACCEPT_INVALID_CERTS").is_some();

    if args.flag_watch {
        warn!("The --watch flag is no longer supported, and will be ignored");
    }
    if env::var_os("WATCH_MODE").is_some() {
        warn!("The WATCH_MODE environment variable is no longer supported, and will be ignored");
    }

    Args {
        arg_connection,
        flag_default_srid,
        flag_ca_root_file,
        flag_danger_accept_invalid_certs,
        ..args
    }
}

async fn start(args: Args) -> io::Result<Server> {
    info!("Starting martin v{VERSION}");

    let (config, pool) = match args.flag_config {
        Some(config_file_name) => {
            info!("Using {config_file_name}");
            setup_from_config(config_file_name).await?
        }
        None => {
            info!("Config is not set");
            setup_from_args(args).await?
        }
    };

    let matches = check_postgis_version(REQUIRED_POSTGIS_VERSION, &pool)
        .await
        .map_err(|e| prettify_error!(e, "Can't check PostGIS version"))?;

    if !matches {
        std::process::exit(-1);
    }

    let listen_addresses = config.listen_addresses.clone();
    let server = server::new(pool, config);
    info!("Martin has been started on {listen_addresses}.");

    Ok(server)
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    let env = env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "martin=info");
    env_logger::Builder::from_env(env).init();

    let args = Docopt::new(USAGE)
        .and_then(|d| d.help(false).deserialize::<Args>())
        .map_err(|e| prettify_error!(e, "Can't parse CLI arguments"))?;

    let args = parse_env(args);

    if args.flag_help {
        println!("{USAGE}");
        std::process::exit(0);
    }

    if args.flag_version {
        println!("v{VERSION}");
        std::process::exit(0);
    }

    if args.flag_danger_accept_invalid_certs {
        warn!("Danger accept invalid certs enabled. You should think very carefully before using this option. If invalid certificates are trusted, any certificate for any site will be trusted for use. This includes expired certificates. This introduces significant vulnerabilities, and should only be used as a last resort.");
    }

    match start(args).await {
        Ok(server) => server.await,
        Err(error) => {
            error!("{error}");
            std::process::exit(-1);
        }
    }
}
