use std::collections::HashMap;
use std::ops::Deref;
use std::time::Duration;

use actix_cors::Cors;
use actix_web::dev::Server;
use actix_web::http::Uri;
use actix_web::middleware::TrailingSlash;
use actix_web::{
    error, middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer, Result,
};
use log::error;
use serde::Deserialize;

use crate::composite_source::CompositeSource;
use crate::config::Config;
use crate::db::{get_connection, Pool};
use crate::function_source::FunctionSources;
use crate::source::{Query, Source, Xyz};
use crate::table_source::{TableSource, TableSources};
use crate::utils::parse_x_rewrite_url;

pub struct AppState {
    pub pool: Pool,
    pub table_sources: Option<TableSources>,
    pub function_sources: Option<FunctionSources>,
    pub default_srid: Option<i32>,
}

#[derive(Deserialize)]
struct SourceRequest {
    source_id: String,
}

#[derive(Deserialize)]
struct CompositeSourceRequest {
    source_ids: String,
}

#[derive(Deserialize)]
struct TileRequest {
    source_id: String,
    z: i32,
    x: i32,
    y: i32,
    #[allow(dead_code)]
    format: String,
}

#[derive(Deserialize)]
struct CompositeTileRequest {
    source_ids: String,
    z: i32,
    x: i32,
    y: i32,
    #[allow(dead_code)]
    format: String,
}

fn map_internal_error<T: std::fmt::Display>(e: T) -> Error {
    // FIXME: is e.to_string() needed here, or can it just be error!("{e}")  ?
    error!("{}", e.to_string());
    error::ErrorInternalServerError(e.to_string())
}

async fn get_health() -> Result<HttpResponse, Error> {
    let response = HttpResponse::Ok().body("OK");
    Ok(response)
}

async fn get_table_sources(state: web::Data<AppState>) -> Result<HttpResponse, Error> {
    Ok(HttpResponse::Ok().json(state.table_sources.as_ref()))
}

async fn get_composite_source(
    req: HttpRequest,
    path: web::Path<CompositeSourceRequest>,
    state: web::Data<AppState>,
) -> Result<HttpResponse> {
    let table_sources = state
        .table_sources
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("There is no table sources"))?;

    let sources: Vec<TableSource> = path
        .source_ids
        .split(',')
        .filter_map(|source_id| table_sources.get(source_id))
        .map(|source| source.deref().clone())
        .collect();

    if sources.is_empty() {
        return Err(error::ErrorNotFound("There is no such table sources"));
    }

    let source = CompositeSource {
        id: path.source_ids.clone(),
        table_sources: sources,
    };

    let mut tilejson = source
        .get_tilejson()
        .await
        .map_err(|e| error::ErrorBadRequest(format!("Can't build TileJSON: {e}")))?;

    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .and_then(parse_x_rewrite_url)
        .unwrap_or_else(|| req.path().trim_end_matches(".json").to_owned());

    let connection_info = req.connection_info();

    let path_and_query = if req.query_string().is_empty() {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}.pbf")
    } else {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}.pbf?{}", req.query_string())
    };

    let tiles_url = Uri::builder()
        .scheme(connection_info.scheme())
        .authority(connection_info.host())
        .path_and_query(path_and_query)
        .build()
        .map(|tiles_url| tiles_url.to_string())
        .map_err(|e| error::ErrorBadRequest(format!("Can't build tiles URL: {e}")))?;

    tilejson.tiles = vec![tiles_url];
    Ok(HttpResponse::Ok().json(tilejson))
}

async fn get_composite_source_tile(
    path: web::Path<CompositeTileRequest>,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let table_sources = state
        .table_sources
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("There is no table sources"))?;

    let sources: Vec<TableSource> = path
        .source_ids
        .split(',')
        .filter_map(|source_id| table_sources.get(source_id))
        .map(|source| source.deref().clone())
        .filter(|src| is_valid_zoom(path.z, src.minzoom, src.maxzoom))
        .collect();

    if sources.is_empty() {
        return Err(error::ErrorNotFound("There is no such table sources"));
    }

    let source = CompositeSource {
        id: path.source_ids.clone(),
        table_sources: sources,
    };

    get_tile(&state, path.z, path.x, path.y, None, Box::new(source)).await
}

async fn get_function_sources(state: web::Data<AppState>) -> Result<HttpResponse, Error> {
    let function_sources = state.function_sources.as_ref();
    Ok(HttpResponse::Ok().json(function_sources))
}

async fn get_function_source(
    req: HttpRequest,
    path: web::Path<SourceRequest>,
    state: web::Data<AppState>,
) -> Result<HttpResponse> {
    let function_sources = state
        .function_sources
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("There is no function sources"))?;

    let source = function_sources.get(&path.source_id).ok_or_else(|| {
        error::ErrorNotFound(format!("Function source '{}' not found", path.source_id))
    })?;

    let mut tilejson = source
        .get_tilejson()
        .await
        .map_err(|e| error::ErrorBadRequest(format!("Can't build TileJSON: {e}")))?;

    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .and_then(parse_x_rewrite_url)
        .unwrap_or_else(|| req.path().trim_end_matches(".json").to_owned());

    let connection_info = req.connection_info();

    let path_and_query = if req.query_string().is_empty() {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}.pbf")
    } else {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}.pbf?{}", req.query_string())
    };

    let tiles_url = Uri::builder()
        .scheme(connection_info.scheme())
        .authority(connection_info.host())
        .path_and_query(path_and_query)
        .build()
        .map(|tiles_url| tiles_url.to_string())
        .map_err(|e| error::ErrorBadRequest(format!("Can't build tiles URL: {e}")))?;

    tilejson.tiles = vec![tiles_url];
    Ok(HttpResponse::Ok().json(tilejson))
}

async fn get_function_source_tile(
    path: web::Path<TileRequest>,
    query: web::Query<HashMap<String, String>>,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let function_sources = state
        .function_sources
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("There is no function sources"))?;

    let source = function_sources
        .get(&path.source_id)
        .filter(|src| is_valid_zoom(path.z, src.minzoom, src.maxzoom))
        .ok_or_else(|| {
            error::ErrorNotFound(format!("Function source '{}' not found", path.source_id))
        })?;

    get_tile(
        &state,
        path.z,
        path.x,
        path.y,
        Some(query.into_inner()),
        source.clone(),
    )
    .await
}

fn is_valid_zoom(zoom: i32, minzoom: Option<u8>, maxzoom: Option<u8>) -> bool {
    let gte_minzoom = minzoom.map_or(true, |minzoom| zoom >= minzoom.into());

    let lte_maxzoom = maxzoom.map_or(true, |maxzoom| zoom <= maxzoom.into());

    gte_minzoom && lte_maxzoom
}

async fn get_tile(
    state: &web::Data<AppState>,
    z: i32,
    x: i32,
    y: i32,
    query: Option<Query>,
    source: Box<dyn Source + Send>,
) -> Result<HttpResponse, Error> {
    let mut connection = get_connection(&state.pool).await?;
    let tile = source
        .get_tile(&mut connection, &Xyz { z, x, y }, &query)
        .await
        .map_err(map_internal_error)?;

    match tile.len() {
        0 => Ok(HttpResponse::NoContent()
            .content_type("application/x-protobuf")
            .body(tile)),
        _ => Ok(HttpResponse::Ok()
            .content_type("application/x-protobuf")
            .body(tile)),
    }
}

pub fn router(cfg: &mut web::ServiceConfig) {
    cfg.route("/healthz", web::get().to(get_health))
        .route("/index.json", web::get().to(get_table_sources))
        .route("/{source_ids}.json", web::get().to(get_composite_source))
        .route(
            "/{source_ids}/{z}/{x}/{y}.{format}",
            web::get().to(get_composite_source_tile),
        )
        .route("/rpc/index.json", web::get().to(get_function_sources))
        .route("/rpc/{source_id}.json", web::get().to(get_function_source))
        .route(
            "/rpc/{source_id}/{z}/{x}/{y}.{format}",
            web::get().to(get_function_source_tile),
        );
}

fn create_state(pool: Pool, config: Config) -> AppState {
    AppState {
        pool,
        table_sources: config.table_sources,
        function_sources: config.function_sources,
        default_srid: config.default_srid,
    }
}

pub fn new(pool: Pool, config: Config) -> Server {
    let keep_alive = config.keep_alive;
    let worker_processes = config.worker_processes;
    let listen_addresses = config.listen_addresses.clone();

    HttpServer::new(move || {
        let state = create_state(pool.clone(), config.clone());

        let cors_middleware = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET"]);

        App::new()
            .app_data(web::Data::new(state))
            .wrap(cors_middleware)
            .wrap(middleware::NormalizePath::new(TrailingSlash::MergeOnly))
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .configure(router)
    })
    .bind(listen_addresses.clone())
    .unwrap_or_else(|_| panic!("Can't bind to {listen_addresses}"))
    .keep_alive(Duration::from_secs(keep_alive as u64))
    .shutdown_timeout(0)
    .workers(worker_processes)
    .run()
}
