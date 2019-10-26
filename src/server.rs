use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use actix::{Actor, Addr, SyncArbiter, SystemRunner};
use actix_web::{
    error, http, middleware, web, App, Either, Error, HttpRequest, HttpResponse, HttpServer, Result,
};
use futures::Future;

use crate::config::Config;
use crate::coordinator_actor::CoordinatorActor;
use crate::db::PostgresPool;
use crate::db_actor::DBActor;
use crate::function_source::FunctionSources;
use crate::messages;
use crate::source::{Source, XYZ};
use crate::table_source::TableSources;
use crate::worker_actor::WorkerActor;

pub struct AppState {
    pub db: Addr<DBActor>,
    pub coordinator: Addr<CoordinatorActor>,
    pub table_sources: Rc<RefCell<Option<TableSources>>>,
    pub function_sources: Rc<RefCell<Option<FunctionSources>>>,
    pub watch_mode: bool,
}

#[derive(Deserialize)]
struct SourceRequest {
    source_id: String,
}

#[derive(Deserialize)]
struct TileRequest {
    source_id: String,
    z: u32,
    x: u32,
    y: u32,
    #[allow(dead_code)]
    format: String,
}

type SourcesResult = Either<HttpResponse, Box<dyn Future<Item = HttpResponse, Error = Error>>>;

fn get_table_sources(state: web::Data<AppState>) -> SourcesResult {
    if !state.watch_mode {
        let table_sources = state.table_sources.borrow().clone();
        let response = HttpResponse::Ok().json(table_sources);
        return Either::A(response);
    }

    info!("Scanning database for table sources");
    let response = state
        .db
        .send(messages::GetTableSources {})
        .from_err()
        .and_then(move |table_sources| match table_sources {
            Ok(table_sources) => {
                state.coordinator.do_send(messages::RefreshTableSources {
                    table_sources: Some(table_sources.clone()),
                });

                Ok(HttpResponse::Ok().json(table_sources))
            }
            Err(_) => Ok(HttpResponse::InternalServerError().into()),
        });

    Either::B(Box::new(response))
}

fn get_table_source(
    req: HttpRequest,
    path: web::Path<SourceRequest>,
    state: web::Data<AppState>,
) -> Result<HttpResponse> {
    let table_sources = state
        .table_sources
        .borrow()
        .clone()
        .ok_or_else(|| error::ErrorNotFound("There is no table sources"))?;

    let source = table_sources.get(&path.source_id).ok_or_else(|| {
        error::ErrorNotFound(format!("Table source '{}' not found", path.source_id))
    })?;

    let mut tilejson = source
        .get_tilejson()
        .map_err(|e| error::ErrorBadRequest(format!("Can't build TileJSON: {}", e)))?;

    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .map_or(Ok(req.path().trim_end_matches(".json")), |header| {
            let header_str = header.to_str()?;
            Ok(header_str.trim_end_matches(".json"))
        })
        .map_err(|e: http::header::ToStrError| {
            error::ErrorBadRequest(format!("Can't build TileJSON: {}", e))
        })?;

    let query_string = req.query_string();
    let query = if query_string.is_empty() {
        query_string.to_owned()
    } else {
        format!("?{}", query_string)
    };

    let connection_info = req.connection_info();

    let tiles_url = format!(
        "{}://{}{}/{{z}}/{{x}}/{{y}}.pbf{}",
        connection_info.scheme(),
        connection_info.host(),
        tiles_path,
        query
    );

    tilejson.tiles = vec![tiles_url];
    Ok(HttpResponse::Ok().json(tilejson))
}

fn get_table_source_tile(
    path: web::Path<TileRequest>,
    state: web::Data<AppState>,
) -> Result<Box<dyn Future<Item = HttpResponse, Error = Error>>> {
    let table_sources = state
        .table_sources
        .borrow()
        .clone()
        .ok_or_else(|| error::ErrorNotFound("There is no table sources"))?;

    let source = table_sources.get(&path.source_id).ok_or_else(|| {
        error::ErrorNotFound(format!("Table source '{}' not found", path.source_id))
    })?;

    let xyz = XYZ {
        z: path.z,
        x: path.x,
        y: path.y,
    };

    let message = messages::GetTile {
        xyz,
        query: None,
        source: source.clone(),
    };

    let response = state
        .db
        .send(message)
        .from_err()
        .and_then(|result| match result {
            Ok(tile) => match tile.len() {
                0 => Ok(HttpResponse::NoContent()
                    .content_type("application/x-protobuf")
                    .body(tile)),
                _ => Ok(HttpResponse::Ok()
                    .content_type("application/x-protobuf")
                    .body(tile)),
            },
            Err(_) => Ok(HttpResponse::InternalServerError().into()),
        });

    Ok(Box::new(response))
}

fn get_function_sources(state: web::Data<AppState>) -> SourcesResult {
    if !state.watch_mode {
        let function_sources = state.function_sources.borrow().clone();
        let response = HttpResponse::Ok().json(function_sources);
        return Either::A(response);
    }

    info!("Scanning database for function sources");
    let response = state
        .db
        .send(messages::GetFunctionSources {})
        .from_err()
        .and_then(move |function_sources| match function_sources {
            Ok(function_sources) => {
                state.coordinator.do_send(messages::RefreshFunctionSources {
                    function_sources: Some(function_sources.clone()),
                });

                Ok(HttpResponse::Ok().json(function_sources))
            }
            Err(_) => Ok(HttpResponse::InternalServerError().into()),
        });

    Either::B(Box::new(response))
}

fn get_function_source(
    req: HttpRequest,
    path: web::Path<SourceRequest>,
    state: web::Data<AppState>,
) -> Result<HttpResponse> {
    let function_sources = state
        .function_sources
        .borrow()
        .clone()
        .ok_or_else(|| error::ErrorNotFound("There is no function sources"))?;

    let source = function_sources.get(&path.source_id).ok_or_else(|| {
        error::ErrorNotFound(format!("Function source '{}' not found", path.source_id))
    })?;

    let mut tilejson = source
        .get_tilejson()
        .map_err(|e| error::ErrorBadRequest(format!("Can't build TileJSON: {}", e)))?;

    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .map_or(Ok(req.path().trim_end_matches(".json")), |header| {
            let header_str = header.to_str()?;
            Ok(header_str.trim_end_matches(".json"))
        })
        .map_err(|e: http::header::ToStrError| {
            error::ErrorBadRequest(format!("Can't build TileJSON: {}", e))
        })?;

    let query_string = req.query_string();
    let query = if query_string.is_empty() {
        query_string.to_owned()
    } else {
        format!("?{}", query_string)
    };

    let connection_info = req.connection_info();

    let tiles_url = format!(
        "{}://{}{}/{{z}}/{{x}}/{{y}}.pbf{}",
        connection_info.scheme(),
        connection_info.host(),
        tiles_path,
        query
    );

    tilejson.tiles = vec![tiles_url];
    Ok(HttpResponse::Ok().json(tilejson))
}

fn get_function_source_tile(
    path: web::Path<TileRequest>,
    query: web::Query<HashMap<String, String>>,
    state: web::Data<AppState>,
) -> Result<Box<dyn Future<Item = HttpResponse, Error = Error>>> {
    let function_sources = state
        .function_sources
        .borrow()
        .clone()
        .ok_or_else(|| error::ErrorNotFound("There is no function sources"))?;

    let source = function_sources.get(&path.source_id).ok_or_else(|| {
        error::ErrorNotFound(format!("Function source '{}' not found", path.source_id))
    })?;

    let xyz = XYZ {
        z: path.z,
        x: path.x,
        y: path.y,
    };

    let message = messages::GetTile {
        xyz,
        query: Some(query.into_inner()),
        source: source.clone(),
    };

    let response = state
        .db
        .send(message)
        .from_err()
        .and_then(|result| match result {
            Ok(tile) => match tile.len() {
                0 => Ok(HttpResponse::NoContent()
                    .content_type("application/x-protobuf")
                    .body(tile)),
                _ => Ok(HttpResponse::Ok()
                    .content_type("application/x-protobuf")
                    .body(tile)),
            },
            Err(_) => Ok(HttpResponse::InternalServerError().into()),
        });

    Ok(Box::new(response))
}

pub fn router(cfg: &mut web::ServiceConfig) {
    cfg.route("/index.json", web::get().to(get_table_sources))
        .route("/{source_id}.json", web::get().to(get_table_source))
        .route(
            "/{source_id}/{z}/{x}/{y}.{format}",
            web::get().to(get_table_source_tile),
        )
        .route("/rpc/index.json", web::get().to(get_function_sources))
        .route("/rpc/{source_id}.json", web::get().to(get_function_source))
        .route(
            "/rpc/{source_id}/{z}/{x}/{y}.{format}",
            web::get().to(get_function_source_tile),
        );
}

fn create_state(
    db: Addr<DBActor>,
    coordinator: Addr<CoordinatorActor>,
    config: Config,
    watch_mode: bool,
) -> AppState {
    let table_sources = Rc::new(RefCell::new(config.table_sources));
    let function_sources = Rc::new(RefCell::new(config.function_sources));

    let worker_actor = WorkerActor {
        table_sources: table_sources.clone(),
        function_sources: function_sources.clone(),
    };

    let worker: Addr<_> = worker_actor.start();
    coordinator.do_send(messages::Connect { addr: worker });

    AppState {
        db: db.clone(),
        coordinator: coordinator.clone(),
        table_sources,
        function_sources,
        watch_mode,
    }
}

pub fn new(pool: PostgresPool, config: Config, watch_mode: bool) -> SystemRunner {
    let sys = actix_rt::System::new("server");

    let db = SyncArbiter::start(3, move || DBActor(pool.clone()));
    let coordinator: Addr<_> = CoordinatorActor::default().start();

    let keep_alive = config.keep_alive;
    let worker_processes = config.worker_processes;
    let listen_addresses = config.listen_addresses.clone();

    HttpServer::new(move || {
        let state = create_state(db.clone(), coordinator.clone(), config.clone(), watch_mode);

        let cors_middleware = middleware::DefaultHeaders::new()
            .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");

        App::new()
            .data(state)
            .wrap(cors_middleware)
            .wrap(middleware::NormalizePath)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .configure(router)
    })
    .bind(listen_addresses.clone())
    .unwrap_or_else(|_| panic!("Can't bind to {}", listen_addresses))
    .keep_alive(keep_alive)
    .shutdown_timeout(0)
    .workers(worker_processes)
    .start();

    sys
}
