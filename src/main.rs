mod database;
mod error;
mod models;

use error::AppResult;

use axum::{extract::{Path, State},
           http::{header::HeaderMap, StatusCode},
           response::{IntoResponse, Redirect},
           routing::{get, post},
           Form, Router, Server};

use axum_prometheus::PrometheusMetricLayer;

use axum_template::RenderHtml;

use serde::Deserialize;
use serde_json::json;

use crate::database::AppState;
use tracing::{debug, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};


#[tokio::main]
async fn main() -> AppResult<()> {
    let redis_host = &std::env::var("LINKSHRINK_REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let redis_port = std::env::var("LINKSHRINK_REDIS_PORT")
                             .unwrap_or_else(|_| "6379".to_string())
                             .parse::<u16>()
                             .unwrap_or(6379);

    let listen_host = &std::env::var("LINKSHRINK_LISTEN_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listen_port = std::env::var("LINKSHRINK_LISTEN_PORT")
                             .unwrap_or_else(|_| "8080".to_string())
                             .parse::<u16>()
                             .unwrap_or(8080);

    tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::from_default_env()
                                  .add_directive("handlebars=info".parse().unwrap())
                                  .add_directive("hyper=info".parse().unwrap()))
                                  .with(tracing_subscriber::fmt::layer())
                                  .init();
    let (prom_layer, metrics_handler) = PrometheusMetricLayer::pair();

    let database = AppState::new(redis_host, redis_port).await?;

    let app = Router::new().route("/", get(root))
                           .route("/favicon.ico", get(favicon))
                           .route("/links", get(get_all_links))
                           .route("/edit/:keyword", get(edit_keyword))
                           .route("/edit/:keyword", post(update_keyword))
                           .route("/:keyword", get(get_keyword))
                           .route("/metrics", get(|| async move { metrics_handler.render() }))
                           .layer(prom_layer)
                           .with_state(database);

    Server::bind(&format!("{listen_host}:{listen_port}").parse().unwrap()).serve(app.into_make_service())
                                                  .await
                                                  .unwrap();

    Ok(())
}

#[instrument]
async fn root(headers: HeaderMap) -> impl IntoResponse {
    let addr = headers.get("X-Real-IP")
                      .or_else(|| headers.get("X-Forwarded-For"))
                      .and_then(|ip| ip.to_str().ok())
                      .unwrap_or("unknown");
    let user_agent = headers.get("User-Agent")
                            .and_then(|ua| ua.to_str().ok())
                            .unwrap_or("User agent unknown");

    format!("hello, {addr}.\n\nUA:{user_agent}.\n\n\nHeaders Map: {headers:#?}")
}

#[instrument]
async fn favicon() -> impl IntoResponse {
    tokio::fs::read("assets/img/favicon.ico").await
                                             .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[instrument(skip(state))]
async fn get_all_links(State(state): State<AppState>) -> AppResult<impl IntoResponse> {
    let shortlinks = state.get_all_shortlinks().await?;

    Ok(RenderHtml("links",
                  state.get_engine(),
                  json!({ "shortlinks": shortlinks })))
}

#[instrument(skip(state))]
async fn edit_keyword(State(state): State<AppState>,
                      Path(keyword): Path<String>)
                      -> AppResult<impl IntoResponse> {
    let mut shortlink = state.get_shortlink(&keyword).await?.unwrap_or_default();

    shortlink.keyword = keyword;
    debug!("edit shortlink: {:?}", shortlink.keyword);

    Ok(RenderHtml("edit",
                  state.get_engine(),
                  json!({
                      "create": shortlink.url.is_empty(),
                      "shortlink": shortlink
                  })))
}

#[derive(Deserialize, Debug)]
struct UpdateForm {
    url: String,
    owner: String,
    description: String,
    #[serde(default)]
    private: bool,
}

#[instrument(skip(state))]
async fn update_keyword(State(state): State<AppState>,
                        Path(keyword): Path<String>,
                        Form(form): Form<UpdateForm>)
                        -> AppResult<impl IntoResponse> {
    let mut existing = state.get_shortlink(&keyword).await?.unwrap_or_default();

    existing.keyword = keyword;
    existing.url = form.url;
    existing.private = form.private;
    existing.owner = form.owner;
    existing.description = form.description;

    state.store_shortlink(existing.clone()).await?;

    Ok(RenderHtml("edit",
                  state.get_engine(),
                  json!({
                      "saved": true,
                      "shortlink": existing
                  })))
}

#[instrument(skip(state))]
async fn get_keyword(State(state): State<AppState>,
                     Path(keyword): Path<String>)
                     -> AppResult<impl IntoResponse> {
    let shortlink = state.get_shortlink(&keyword).await?;

    let path = match shortlink {
        Some(shortlink) => {
            // update the state
            state.hit_shortlink(&keyword).await?;

            shortlink.url
        }
        None => format!("/edit/{keyword}"),
    };

    Ok(Redirect::temporary(&path))
}
