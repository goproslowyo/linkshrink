use axum::{extract::FromRef,
           http::StatusCode,
           response::{IntoResponse, Response}};
use axum_template::engine::Engine;
use handlebars::Handlebars;
use lockfree::map::Map as LFMap;
use std::{sync::Arc, time::Duration};

use crate::models::Shortlink;
use redis_async::{client,
                  client::PairedConnection,
                  error::Error as RedisError,
                  resp::{FromResp, RespValue},
                  resp_array};
use serde::Serialize;
use tap::TapFallible;
use thiserror::Error;
use tracing::{debug, error, info, instrument};

pub type TemplateEngine = Engine<Handlebars<'static>>;
pub type RedisConnection = Arc<PairedConnection>;

const FLUSH_SLEEP_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, FromRef)]
pub struct AppState {
    cache: Arc<LFMap<String, Shortlink>>,
    engine: TemplateEngine,
    connection: RedisConnection,
}

impl AppState {
    pub fn get_engine(&self) -> TemplateEngine {
        self.engine.clone()
    }

    #[instrument]
    pub async fn new(host: &str, port: u16) -> Result<Self, DatabaseError> {
        let connection =
            client::paired_connect(host, port).await
                                              .tap_err(|err| {
                                                  error!("Failed to connect to redis: {err:#?}")
                                              })
                                              .map_err(|_| DatabaseError::UnableToConnect)?;

        let mut handlebars = Handlebars::default();
        handlebars.register_templates_directory(".html.hbs", "templates/")
                  .tap_err(|err| error!("Failed to register handlebar templates: {err:#?}"))
                  .expect("Failed to register handlebar templates");

        let state = Self { cache: Arc::new(LFMap::default()),
                           engine: Engine::from(handlebars),
                           connection: Arc::new(connection) };

        let weak_cache = Arc::downgrade(&state.cache);
        let weak_connection = Arc::downgrade(&state.connection);

        tokio::spawn(async move {
            loop {
                debug!("Flushing shortlink hits from cache to redis.");

                match (weak_cache.upgrade(), weak_connection.upgrade()) {
                    (Some(strong_cache), Some(strong_connection)) => {
                        let shortlink_values =
                            strong_cache.iter().filter_map(|entry| {
                                                   entry.val().set_key_in_redis().ok()
                                               });

                        for shortlink_value in shortlink_values {
                            // todo: update this to use streams buffered unordered
                            let _ = strong_connection.send::<()>(shortlink_value).await;
                        }
                    }
                    _ => {
                        info!("Cache or redis connection dropped. Background flush ending...");
                        break;
                    }
                }

                tokio::time::sleep(FLUSH_SLEEP_DURATION).await;
            }
        });

        Ok(state)
    }

    #[instrument(skip(self))]
    /// get all shortlinks
    pub async fn get_all_shortlinks(&self) -> Result<Vec<Shortlink>, DatabaseError> {
        let keys: Vec<String> = self.connection
                                    .send(resp_array!["KEYS", "sl::*"])
                                    .await
                                    .map_err(|_| DatabaseError::FailedToQueryRedis)?;

        info!(keys_found = keys.len());

        if keys.is_empty() {
            return Ok(vec![]);
        }

        let resp_keys = keys.iter()
                            .map(|key| key.into())
                            .collect::<Vec<RespValue>>();

        let mut mget_query = vec!["MGET".into()];
        mget_query.extend(resp_keys);

        let cache_fetch = keys.iter().map(|key| self.cache.get(&key[4..]));

        // todo: fetch from cache and determine _which_ keys we actually need to fetch from redis
        let shortlink_results =
            self.connection
                .send::<Vec<Shortlink>>(RespValue::Array(mget_query))
                .await
                .map_err(|_| DatabaseError::FailedToQueryRedis)?
                .into_iter()
                .zip(cache_fetch)
                .map(|(redis_shortlink, maybe_cache_shortlink)| match maybe_cache_shortlink {
                         Some(cached_shortlink) => cached_shortlink.val().clone(),
                         _ => redis_shortlink,
                     })
                .collect::<Vec<Shortlink>>();

        Ok(shortlink_results)
    }

    #[instrument(skip(self))]
    /// get a shortlink
    pub async fn get_shortlink(&self, keyword: &str) -> Result<Option<Shortlink>, DatabaseError> {
        if let Some(value) = self.cache.get(keyword) {
            info!("Fetched from cache.");
            return Ok(Some(value.val().clone()));
        }

        let entry = self.connection
                        .send::<Option<Shortlink>>(resp_array!["GET", format!("sl::{keyword}")])
                        .await
                        .map_err(|_| DatabaseError::FailedToQueryRedis)?;

        if let Some(shortlink) = &entry {
            info!("Entry set into cache");
            self.cache.insert(keyword.to_string(), shortlink.clone());
        }

        Ok(entry)
    }

    pub async fn hit_shortlink(&self, keyword: &str) -> Result<(), DatabaseError> {
        let mut shortlink: Shortlink = self.get_shortlink(keyword)
                                           .await?
                                           .expect("Cannot find shortlink");
        shortlink.hits += 1;
        // only update the cache with the hit
        self.cache.insert(keyword.to_string(), shortlink);
        Ok(())
    }

    /// save a shortlink
    pub async fn store_shortlink(&self, mut shortlink: Shortlink) -> Result<(), DatabaseError> {
        if let Some(existing_cache) = self.cache.get(&shortlink.keyword) {
            info!("Shortlink found in cache");
            shortlink.hits = existing_cache.val().hits;
        }

        self.connection
            .send(shortlink.set_key_in_redis()?)
            .await
            .map_err(|_| DatabaseError::FailedToQueryRedis)?;

        self.cache
            .insert(shortlink.keyword.clone(), shortlink.clone());

        Ok(())
    }
}

#[derive(Debug, Serialize, Error)]
pub enum DatabaseError {
    #[error("Unable to connect")]
    UnableToConnect,
    #[error("Failed to query redis")]
    FailedToQueryRedis,
    #[error("Failed to evict cache")]
    FailedToEvictCache,
}

impl IntoResponse for DatabaseError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

impl FromResp for Shortlink {
    fn from_resp_int(resp: RespValue) -> Result<Self, RedisError> {
        let serialized = String::from_resp(resp)?;

        serde_json::from_str::<Shortlink>(&serialized)
            .map_err(|_| RedisError::Unexpected("Failed to deserialize data".to_string()))
    }
}

impl Shortlink {
    fn set_key_in_redis(&self) -> Result<RespValue, DatabaseError> {
        let keyword = self.keyword.clone();
        let serialized =
            serde_json::to_string(self).map_err(|_| DatabaseError::FailedToQueryRedis)?;

        Ok(resp_array!["SET", format!("sl::{keyword}"), serialized])
    }
}
