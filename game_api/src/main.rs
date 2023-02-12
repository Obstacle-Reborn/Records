use self::graphql::create_schema;
use actix_cors::Cors;
use actix_web::{web::Data, App, HttpServer};
use anyhow::Context;
use deadpool::Runtime;
use sqlx::mysql;
use std::time::Duration;
use tracing_actix_web::TracingLogger;
use tracing_subscriber::fmt::format::FmtSpan;
use warp::http::header;

pub mod graphql;
pub mod http;
pub mod xml;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Filter traces based on the RUST_LOG env var, or, if it's not set,
    // default to show the output of the example.
    let filter = std::env::var("RECORDS_API_LOG")
        .unwrap_or_else(|_| "tracing=info,warp=info,game_api=info".to_owned());

    // let mut port = 3000 as u16;
    let mut port = 3001 as u16;
    if let Ok(s) = std::env::var("RECORDS_API_PORT") {
        if let Ok(env_port) = s.parse::<u16>() {
            port = env_port;
        }
    };

    let mysql_pool = mysql::MySqlPoolOptions::new()
        .acquire_timeout(Duration::new(10, 0))
        .connect("mysql://records_api:api@localhost/obs_records")
        // .connect("mysql://root:root@localhost/obstacle_records")
        .await?;

    let redis_pool = {
        let cfg = deadpool_redis::Config {
            // url: Some("redis://10.0.0.1/".to_string()),
            url: Some("redis://127.0.0.1:6379/".to_string()),
            // url: Some("redis://localhost/".to_string()),
            connection: None,
            pool: None,
        };
        cfg.create_pool(Some(Runtime::Tokio1)).unwrap()
    };

    let db = records_lib::Database {
        mysql_pool,
        redis_pool,
    };

    // Configure the default `tracing` subscriber.
    // The `fmt` subscriber from the `tracing-subscriber` crate logs `tracing`
    // events to stdout. Other subscribers are available for integrating with
    // distributed tracing systems such as OpenTelemetry.
    tracing_subscriber::fmt()
        // Use the filter we built above to determine which traces to record.
        .with_env_filter(filter)
        // Record an event when each span closes. This can be used to time our
        // routes' durations!
        .with_span_events(FmtSpan::CLOSE)
        .init();

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            // .allowed_origin("https://www.obstacle.ovh")
            .allowed_methods(vec!["GET", "POST"])
            .allowed_headers(vec![header::ACCEPT, header::CONTENT_TYPE])
            .max_age(3600);

        App::new()
            .wrap(cors)
            .wrap(TracingLogger::default())
            .app_data(Data::new(create_schema(db.clone())))
            .app_data(Data::new(db.clone()))
            .service(graphql::index_playground)
            .service(graphql::index_graphql)
            .service(http::overview)
            .service(http::overview_compat)
            .service(http::update_player)
            .service(http::update_player_compat)
            .service(http::update_map)
            .service(http::update_map_compat)
            .service(http::player_finished)
            .service(http::player_finished_compat)
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
    .context("Failed to run server")
}