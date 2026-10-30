use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize)]
struct Item {
    id: Uuid,
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct CreateItem {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct UpdateItem {
    name: Option<String>,
    description: Option<String>,
}

type Db = Arc<RwLock<HashMap<Uuid, Item>>>;

pub fn router() -> Router {
    Router::new()
        .route("/items", get(list_items).post(create_item))
        .route(
            "/items/{id}",
            get(get_item).put(update_item).delete(delete_item),
        )
        .with_state(Db::default())
}

async fn list_items(State(db): State<Db>) -> Json<Vec<Item>> {
    let items = db.read().unwrap().values().cloned().collect();
    Json(items)
}

async fn create_item(
    State(db): State<Db>,
    Json(input): Json<CreateItem>,
) -> (StatusCode, Json<Item>) {
    let item = Item {
        id: Uuid::new_v4(),
        name: input.name,
        description: input.description,
    };
    db.write().unwrap().insert(item.id, item.clone());
    (StatusCode::CREATED, Json(item))
}

async fn get_item(Path(id): Path<Uuid>, State(db): State<Db>) -> Result<Json<Item>, StatusCode> {
    db.read()
        .unwrap()
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn update_item(
    Path(id): Path<Uuid>,
    State(db): State<Db>,
    Json(input): Json<UpdateItem>,
) -> Result<Json<Item>, StatusCode> {
    let mut db = db.write().unwrap();
    let item = db.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;

    if let Some(name) = input.name {
        item.name = name;
    }
    if let Some(description) = input.description {
        item.description = Some(description);
    }

    Ok(Json(item.clone()))
}

async fn delete_item(Path(id): Path<Uuid>, State(db): State<Db>) -> StatusCode {
    match db.write().unwrap().remove(&id) {
        Some(_) => StatusCode::NO_CONTENT,
        None => StatusCode::NOT_FOUND,
    }
}
