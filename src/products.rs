use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::{JsonRejection, PathRejection, QueryRejection}},
    http::StatusCode,
    routing::get,
};
use futures_util::TryStreamExt;
use mongodb::{
    Collection,
    bson::{DateTime, Document, doc, oid::ObjectId},
    options::ReturnDocument,
};
use serde::{Deserialize, Serialize};

use crate::{db::PRODUCTS, error::{AppError, is_duplicate_key}, retry::RetryPolicy};

const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 100;

#[derive(Clone)]
pub struct ProductsState {
    pub coll: Collection<ProductDoc>,
    pub retry: RetryPolicy,
}

pub fn router(db: &mongodb::Database, retry: RetryPolicy) -> Router {
    let state = ProductsState { coll: db.collection(PRODUCTS), retry };
    Router::new()
        .route("/products", get(list_products).post(create_product))
        .route(
            "/products/{id}",
            get(get_product).put(update_product).delete(delete_product),
        )
        .with_state(state)
}

/// Document as stored in MongoDB.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProductDoc {
    #[serde(rename = "_id")]
    id: ObjectId,
    sku: String,
    name: String,
    price: f64,
    category: Option<String>,
    created_at: DateTime,
    updated_at: DateTime,
}

/// Representation returned by the API.
#[derive(Serialize)]
struct Product {
    id: String,
    sku: String,
    name: String,
    price: f64,
    category: Option<String>,
    created_at: String,
    updated_at: String,
}

impl From<ProductDoc> for Product {
    fn from(d: ProductDoc) -> Self {
        Self {
            id: d.id.to_hex(),
            sku: d.sku,
            name: d.name,
            price: d.price,
            category: d.category,
            created_at: d.created_at.try_to_rfc3339_string().unwrap_or_default(),
            updated_at: d.updated_at.try_to_rfc3339_string().unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
struct CreateProduct {
    sku: String,
    name: String,
    price: f64,
    category: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProduct {
    name: Option<String>,
    price: Option<f64>,
    category: Option<String>,
}

#[derive(Deserialize)]
struct ListParams {
    page: Option<u64>,
    limit: Option<u64>,
    category: Option<String>,
    sort: Option<String>,
}

#[derive(Serialize)]
struct Page<T> {
    data: Vec<T>,
    pagination: Pagination,
}

#[derive(Serialize)]
struct Pagination {
    page: u64,
    limit: u64,
    total_items: u64,
    total_pages: u64,
    has_next: bool,
    has_prev: bool,
}

fn parse_id(id: &str) -> Result<ObjectId, AppError> {
    ObjectId::parse_str(id)
        .map_err(|_| AppError::BadRequest(format!("`{id}` is not a valid product id")))
}

fn not_found(id: ObjectId) -> AppError {
    AppError::NotFound(format!("product `{id}` not found"))
}

fn validate_name(name: &str) -> Result<(), AppError> {
    if name.trim().is_empty() {
        return Err(AppError::Validation("`name` must not be empty".into()));
    }
    Ok(())
}

fn validate_price(price: f64) -> Result<(), AppError> {
    if !price.is_finite() || price < 0.0 {
        return Err(AppError::Validation("`price` must be a non-negative number".into()));
    }
    Ok(())
}

fn parse_sort(sort: Option<&str>) -> Result<Document, AppError> {
    let raw = sort.unwrap_or("-created_at");
    let (field, dir) = match raw.strip_prefix('-') {
        Some(f) => (f, -1),
        None => (raw, 1),
    };
    if !["created_at", "price", "name"].contains(&field) {
        return Err(AppError::BadRequest(format!(
            "invalid sort `{raw}`; allowed: created_at, price, name (prefix with `-` for descending)"
        )));
    }
    // `_id` as tie-breaker keeps page boundaries stable.
    Ok(doc! { field: dir, "_id": dir })
}

async fn list_products(
    State(s): State<ProductsState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Page<Product>>, AppError> {
    let Query(params) = params?;
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if page == 0 {
        return Err(AppError::BadRequest("`page` must be >= 1".into()));
    }
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(AppError::BadRequest(format!("`limit` must be between 1 and {MAX_LIMIT}")));
    }
    let skip = (page - 1)
        .checked_mul(limit)
        .ok_or_else(|| AppError::BadRequest("`page` is too large".into()))?;
    let sort = parse_sort(params.sort.as_deref())?;
    let filter = match params.category {
        Some(c) => doc! { "category": c },
        None => doc! {},
    };

    let count = s.retry.run("products.count", || {
        s.coll.count_documents(filter.clone())
    });
    let find = s.retry.run("products.find", || {
        let (coll, filter, sort) = (s.coll.clone(), filter.clone(), sort.clone());
        async move {
            coll.find(filter)
                .sort(sort)
                .skip(skip)
                .limit(limit as i64)
                .await?
                .try_collect::<Vec<_>>()
                .await
        }
    });
    let (total_items, docs) = tokio::try_join!(count, find)?;

    let total_pages = total_items.div_ceil(limit);
    Ok(Json(Page {
        data: docs.into_iter().map(Product::from).collect(),
        pagination: Pagination {
            page,
            limit,
            total_items,
            total_pages,
            has_next: page < total_pages,
            has_prev: page > 1,
        },
    }))
}

async fn create_product(
    State(s): State<ProductsState>,
    input: Result<Json<CreateProduct>, JsonRejection>,
) -> Result<(StatusCode, Json<Product>), AppError> {
    let Json(input) = input?;
    if input.sku.trim().is_empty() {
        return Err(AppError::Validation("`sku` must not be empty".into()));
    }
    validate_name(&input.name)?;
    validate_price(input.price)?;

    let now = DateTime::now();
    // The `_id` is generated client-side so retrying the insert is idempotent.
    let product = ProductDoc {
        id: ObjectId::new(),
        sku: input.sku,
        name: input.name,
        price: input.price,
        category: input.category,
        created_at: now,
        updated_at: now,
    };

    let retried = AtomicBool::new(false);
    s.retry
        .run("products.insert", || {
            let (coll, product) = (s.coll.clone(), product.clone());
            let is_retry = retried.swap(true, Ordering::Relaxed);
            async move {
                match coll.insert_one(&product).await {
                    Ok(_) => Ok(()),
                    // A previous attempt may have succeeded before timing out.
                    Err(e) if is_retry && is_duplicate_key(&e) => {
                        match coll.find_one(doc! { "_id": product.id }).await? {
                            Some(_) => Ok(()),
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                }
            }
        })
        .await?;

    Ok((StatusCode::CREATED, Json(product.into())))
}

async fn get_product(
    State(s): State<ProductsState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<Product>, AppError> {
    let id = parse_id(&id?.0)?;
    s.retry
        .run("products.find_one", || s.coll.find_one(doc! { "_id": id }))
        .await?
        .map(|d| Json(d.into()))
        .ok_or_else(|| not_found(id))
}

async fn update_product(
    State(s): State<ProductsState>,
    id: Result<Path<String>, PathRejection>,
    input: Result<Json<UpdateProduct>, JsonRejection>,
) -> Result<Json<Product>, AppError> {
    let id = parse_id(&id?.0)?;
    let Json(input) = input?;

    let mut set = Document::new();
    if let Some(name) = input.name {
        validate_name(&name)?;
        set.insert("name", name);
    }
    if let Some(price) = input.price {
        validate_price(price)?;
        set.insert("price", price);
    }
    if let Some(category) = input.category {
        set.insert("category", category);
    }
    if set.is_empty() {
        return Err(AppError::Validation(
            "provide at least one of `name`, `price`, `category`".into(),
        ));
    }
    set.insert("updated_at", DateTime::now());
    let update = doc! { "$set": set };

    s.retry
        .run("products.update", || {
            s.coll
                .find_one_and_update(doc! { "_id": id }, update.clone())
                .return_document(ReturnDocument::After)
        })
        .await?
        .map(|d| Json(d.into()))
        .ok_or_else(|| not_found(id))
}

async fn delete_product(
    State(s): State<ProductsState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, AppError> {
    let id = parse_id(&id?.0)?;
    let res = s
        .retry
        .run("products.delete", || s.coll.delete_one(doc! { "_id": id }))
        .await?;
    match res.deleted_count {
        0 => Err(not_found(id)),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}
