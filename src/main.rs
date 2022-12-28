mod database;
mod error;
mod models;

use error::AppResult;

use axum::{extract::{Path, State},
           http::StatusCode,
           response::{IntoResponse, Redirect},
           routing::{get, post},
           Form, Router, Server};

use axum_template::RenderHtml;

use serde::Deserialize;
use serde_json::json;

use crate::database::AppState;
use tracing::instrument;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> AppResult<()> {
    tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::from_default_env())
                                  .with(tracing_subscriber::fmt::layer())
                                  .init();

    let database = AppState::new("0.0.0.0", 6379).await?;

    let app = Router::new().route("/", get(root))
                           .route("/favicon.ico", get(favicon))
                           .route("/links", get(get_all_links))
                           .route("/edit/:keyword", get(edit_keyword))
                           .route("/edit/:keyword", post(update_keyword))
                           .route("/:keyword", get(get_keyword))
                           .with_state(database);

    Server::bind(&"0.0.0.0:8080".parse().unwrap()).serve(app.into_make_service())
                                                  .await
                                                  .unwrap();

    Ok(())
}

#[instrument]
async fn root() -> impl IntoResponse {
    "hello, world"
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
