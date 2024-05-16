use super::AppState;
use crate::error::Error;
use crate::{DOMAIN, HTTPS_DOMAIN};
use axum::extract::State;
use axum::Json;
use axum_macros::debug_handler;
use serde_json::json;
use std::sync::Arc;
use tracing::info;

#[debug_handler]
#[tracing::instrument(skip_all)]
pub async fn nodeinfo(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Error> {
    info!("nodeinfo");
    Ok(Json(json!({
        "version": "2.1",
        "software": {
            "name": "momostr",
            "version": "0.1.0",
        },
        "protocols": [
            "activitypub"
        ],
        "services": {
            "inbound": [],
            "outbound": []
        },
        "openRegistrations": false,
        "usage": {
            "users": {
                "total": state.nostr_account_to_followers.lock().len(),
            },
        },
        "metadata": {
            "nodeName": DOMAIN,
            "nodeDescription": "A WIP bridge between Nostr and Fediverse.",
            "langs": [],
            "themeColor": "#ff549e"
        }
    })))
}

pub async fn well_known_nodeinfo() -> Result<Json<serde_json::Value>, Error> {
    info!("well_known_nodeinfo");
    Ok(Json(json!({
        "links": [
            {
                "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                "href": format_args!("{HTTPS_DOMAIN}/nodeinfo/2.1")
            }
        ]
    })))
}
