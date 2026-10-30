# Catalog API

A REST HTTP service written in Rust with [axum](https://github.com/tokio-rs/axum), [tokio](https://tokio.rs/) and the official [MongoDB driver](https://github.com/mongodb/mongo-rust-driver).

It exposes two resources:

| Resource    | Storage   | Notes                                                                          |
|-------------|-----------|--------------------------------------------------------------------------------|
| `/items`    | In memory | Simple CRUD. Data is lost when the server stops.                               |
| `/products` | MongoDB   | CRUD with pagination, filtering, sorting, connection pooling, retries and JSON errors. |

---

## Table of contents

- [Getting started](#getting-started)
- [Configuration](#configuration)
- [Project structure](#project-structure)
- [Resilience and connection pool](#resilience-and-connection-pool)
- [Rate limiting](#rate-limiting)
- [Health endpoints](#health-endpoints)
- [Products API (MongoDB)](#products-api-mongodb)
- [Items API (in memory)](#items-api-in-memory)
- [Error responses](#error-responses)
- [End-to-end walkthroughs](#end-to-end-walkthroughs)

---

## Getting started

### Requirements

- Rust toolchain (edition 2024) with `cargo`
- A MongoDB server (only needed for `/products`)

### Start MongoDB

With Docker:

```sh
docker run -d --name catalog-mongo -p 27017:27017 mongo:7
```

With Podman:

```sh
podman run -d --name catalog-mongo -p 27017:27017 docker.io/library/mongo:7
```

### Run the service

```sh
cargo run
```

```
INFO catalog_api::db: mongodb client configured db="catalog" max_pool_size=20 min_pool_size=2
INFO catalog_api: listening on 0.0.0.0:3000
INFO catalog_api::db: mongodb indexes ensured
```

The server **starts even if MongoDB is down**: `/items` and `/health` keep working and `/products`
returns `503` while the database is unreachable. In the background it tries to create the indexes up to
`MONGO_STARTUP_MAX_ATTEMPTS` times (default `10`); if MongoDB is still unreachable after that, the
service shuts down gracefully, closes the connection pool and exits with code `1`, so the container
orchestrator can restart it or alert instead of looping forever.

Stop it with `Ctrl+C`. The server shuts down gracefully (in-flight requests finish) and then closes the MongoDB connection pool.

### Build a release binary

```sh
cargo build --release
./target/release/catalog-api
```

---

## Configuration

All settings are read from environment variables. Invalid values are logged and replaced by the default.

| Variable                            | Default                     | Description                                                     |
|-------------------------------------|-----------------------------|-----------------------------------------------------------------|
| `BIND_ADDR`                         | `0.0.0.0:3000`              | Address the HTTP server listens on.                             |
| `MONGODB_URI`                       | `mongodb://localhost:27017` | MongoDB connection string (supports `mongodb+srv://`, auth, replica sets). |
| `MONGODB_DB`                        | `catalog`                   | Database name.                                                  |
| `MONGO_MAX_POOL_SIZE`               | `20`                        | Maximum connections in the pool.                                |
| `MONGO_MIN_POOL_SIZE`               | `2`                         | Connections kept open even when idle.                           |
| `MONGO_MAX_IDLE_SECS`               | `300`                       | Idle connections older than this are closed.                    |
| `MONGO_CONNECT_TIMEOUT_MS`          | `5000`                      | Timeout to open a TCP connection.                               |
| `MONGO_SERVER_SELECTION_TIMEOUT_MS` | `3000`                      | How long the driver waits for an available server.              |
| `MONGO_OP_TIMEOUT_MS`               | `5000`                      | Timeout for each attempt of a database operation.               |
| `MONGO_STARTUP_MAX_ATTEMPTS`        | `10`                        | Attempts to reach MongoDB at startup (index creation). If all fail, the service shuts down with exit code `1`. |
| `RETRY_MAX_ATTEMPTS`                | `3`                         | Total attempts (first try + retries) for transient errors.      |
| `RETRY_BASE_DELAY_MS`               | `100`                       | Initial backoff delay.                                          |
| `RETRY_MAX_DELAY_MS`                | `2000`                      | Maximum backoff delay.                                          |
| `RATE_LIMIT_ENABLED`                | `true`                      | Enables per-client rate limiting on every route.                |
| `RATE_LIMIT_PER_SECOND`             | `10`                        | Sustained requests per second per client (decimals allowed, e.g. `0.5`). `0` disables it. |
| `RATE_LIMIT_BURST`                  | `20`                        | Maximum requests a client can send at once.                     |
| `RATE_LIMIT_TRUST_PROXY`            | `false`                     | Identify clients by the first `X-Forwarded-For` address. Only enable behind a trusted proxy. |
| `RUST_LOG`                          | `info,mongodb=warn`         | Log filter (e.g. `debug`, `catalog_api=debug`).                    |

Example:

```sh
MONGODB_URI="mongodb://user:pass@localhost:27017/?authSource=admin" \
MONGO_MAX_POOL_SIZE=50 \
RETRY_MAX_ATTEMPTS=5 \
RUST_LOG=debug \
cargo run
```

---

## Project structure

```
src/
├── main.rs      # Startup, routing, health endpoints, graceful shutdown
├── config.rs    # Environment-based configuration
├── db.rs        # MongoDB client + connection pool, ping, index creation
├── ratelimit.rs # Per-client-IP rate limiting middleware (token bucket)
├── retry.rs     # Retry policy: transient error detection, timeouts, backoff
├── error.rs     # AppError -> HTTP status + JSON body
├── products.rs  # /products handlers (MongoDB, paginated)
└── items.rs     # /items handlers (in memory)
```

---

## Resilience and connection pool

### Connection pool

A single `mongodb::Client` is created at startup and shared by every request. The client owns a
connection pool per server, configured with `MONGO_MAX_POOL_SIZE`, `MONGO_MIN_POOL_SIZE`,
`MONGO_MAX_IDLE_SECS`, connect and server-selection timeouts. Requests borrow a connection
and return it automatically; when all connections are busy, requests wait for a free one.
On shutdown, `client.shutdown()` closes the pool cleanly.

### Retries

Every MongoDB operation on `/products` goes through a `RetryPolicy`:

1. Each attempt is bounded by `MONGO_OP_TIMEOUT_MS`.
2. If the attempt fails with a **transient** error, it is retried up to `RETRY_MAX_ATTEMPTS` times.
3. Between attempts it waits with **exponential backoff with jitter**:
   `min(base * 2^(attempt-1), max)`, scaled by a random factor in `[0.5, 1.0)`.
4. Non-transient errors (validation, duplicate key, ...) are returned immediately, without retries.

Transient errors are network/IO errors, server-selection timeouts, cleared pools, errors with the
`RetryableWriteError`, `TransientTransactionError` or `SystemOverloadedError` labels, and server
codes such as `HostUnreachable`, `NetworkTimeout`, `PrimarySteppedDown`, `NotWritablePrimary`,
`ShutdownInProgress`, etc.

The driver's own `retryReads` / `retryWrites` are enabled too (effective on replica sets).

### Idempotency

Retries are safe because all operations are idempotent:

- **Create**: the `_id` is generated by the service *before* the first attempt. If a retry hits a
  duplicate key, the service checks whether its own document already exists (the previous attempt
  succeeded but timed out) and, if so, treats it as success instead of returning `409`.
- **Update**: a `$set` applied twice gives the same result.
- **Read / delete**: by `_id`.

### Graceful degradation

- The server starts even if MongoDB is down; the driver connects lazily.
- Indexes (`sku` unique, `category + created_at`) are created by a background task that retries
  up to `MONGO_STARTUP_MAX_ATTEMPTS` times (backoff of at least 1 s between attempts). If every
  attempt fails, or the error is not transient (e.g. bad credentials), the server stops accepting
  connections, waits for in-flight requests, closes the MongoDB pool and exits with code `1`.
  With the defaults this takes roughly 40–50 s (10 × (3 s server selection + backoff)).
- Once the indexes exist, a later outage does **not** stop the service: `/products` returns `503` with a `Retry-After: 5` header, while `/items` and
  `/health` keep working. Once MongoDB is back, requests succeed again without restarting.
- With the defaults, a request during an outage takes about 9 s to fail
  (3 attempts × 3 s server-selection timeout). Lower `MONGO_SERVER_SELECTION_TIMEOUT_MS` or
  `RETRY_MAX_ATTEMPTS` to fail faster.

### Error hygiene

Internal error details are logged on the server but never sent to the client;
`500` responses only contain a generic message.

---

## Rate limiting

Every request (including `/health`) goes through a **token bucket per client IP**: each client
can send up to `RATE_LIMIT_BURST` requests at once, and the bucket refills at
`RATE_LIMIT_PER_SECOND` tokens per second. Buckets live in memory, per process (with several
replicas, each one limits independently).

Allowed responses include `X-RateLimit-Limit` and `X-RateLimit-Remaining` headers. When the limit
is exceeded:

**`429 Too Many Requests`** with headers `Retry-After: <seconds>`, `X-RateLimit-Remaining: 0`

```json
{ "error": { "code": "RATE_LIMITED", "message": "too many requests, slow down" } }
```

```sh
RATE_LIMIT_PER_SECOND=1 RATE_LIMIT_BURST=3 cargo run
for i in 1 2 3 4 5; do curl -s -o /dev/null -w "%{http_code} " http://localhost:3000/health; done
# 200 200 200 429 429
```

---

## Health endpoints

### Liveness

`GET /health` — is the process up? Does not touch the database.

```sh
curl -i http://localhost:3000/health
```

**`200 OK`**

```json
{ "status": "ok" }
```

### Readiness

`GET /health/ready` — can the service reach MongoDB? Sends a `ping` with a 2 s timeout.

```sh
curl -i http://localhost:3000/health/ready
```

**`200 OK`**

```json
{ "status": "ok", "mongodb": "up" }
```

**`503 Service Unavailable`** (MongoDB unreachable)

```json
{ "status": "degraded", "mongodb": "timeout" }
```

`mongodb` is `"down"` when the ping fails with an error, or `"timeout"` when it does not answer within 2 s.

---

## Products API (MongoDB)

### Data model

| Field         | Type                  | Description                                      |
|---------------|-----------------------|--------------------------------------------------|
| `id`          | string (ObjectId hex) | Generated by the service.                        |
| `sku`         | string                | Unique product code. Required, cannot be changed.|
| `name`        | string                | Required, not empty.                             |
| `price`       | number                | Required, `>= 0`.                                |
| `category`    | string \| null        | Optional. Can be used to filter.                 |
| `created_at`  | string (RFC 3339)     | Set on creation.                                 |
| `updated_at`  | string (RFC 3339)     | Updated on every change.                         |

```json
{
  "id": "6ae4364d9a2c8b0793e0b765",
  "sku": "SKU-25",
  "name": "Product 25",
  "price": 25.5,
  "category": "odd",
  "created_at": "2026-10-30T06:16:45.873Z",
  "updated_at": "2026-10-30T06:16:45.873Z"
}
```

### Endpoints overview

| Method   | Path              | Description                      | Success status   |
|----------|-------------------|----------------------------------|------------------|
| `GET`    | `/products`       | List products (paginated)        | `200 OK`         |
| `POST`   | `/products`       | Create a product                 | `201 Created`    |
| `GET`    | `/products/{id}`  | Get one product                  | `200 OK`         |
| `PUT`    | `/products/{id}`  | Partially update a product       | `200 OK`         |
| `DELETE` | `/products/{id}`  | Delete a product                 | `204 No Content` |

---

### 1. List products (paginated)

`GET /products`

**Query parameters**

| Parameter  | Type    | Default       | Description                                                           |
|------------|---------|---------------|-----------------------------------------------------------------------|
| `page`     | integer | `1`           | Page number, starting at 1.                                           |
| `limit`    | integer | `20`          | Items per page, between `1` and `100`.                                |
| `category` | string  | —             | Only return products of this category.                                |
| `sort`     | string  | `-created_at` | `created_at`, `price` or `name`. Prefix with `-` for descending order.|

Results are always ordered by `_id` as a tie-breaker, so page boundaries are stable.
The total count and the page are fetched from MongoDB concurrently.

**curl**

```sh
# First page with defaults (20 newest products)
curl -i "http://localhost:3000/products"

# Page 2, 10 per page
curl -i "http://localhost:3000/products?page=2&limit=10"

# Cheapest first
curl -i "http://localhost:3000/products?sort=price"

# Most expensive first, only category "even"
curl -i "http://localhost:3000/products?category=even&sort=-price&limit=5"

# Alphabetical
curl -i "http://localhost:3000/products?sort=name"
```

**Response — `200 OK`**

```json
{
  "data": [
    {
      "id": "6ae4364d9a2c8b0793e0b761",
      "sku": "SKU-21",
      "name": "Product 21",
      "price": 21.5,
      "category": "odd",
      "created_at": "2026-10-30T06:16:45.840Z",
      "updated_at": "2026-10-30T06:16:45.840Z"
    },
    {
      "id": "6ae4364d9a2c8b0793e0b762",
      "sku": "SKU-22",
      "name": "Product 22",
      "price": 22.5,
      "category": "even",
      "created_at": "2026-10-30T06:16:45.848Z",
      "updated_at": "2026-10-30T06:16:45.848Z"
    }
  ],
  "pagination": {
    "page": 3,
    "limit": 10,
    "total_items": 25,
    "total_pages": 3,
    "has_next": false,
    "has_prev": true
  }
}
```

A page beyond the last one returns an empty `data` array (with the real `total_items`/`total_pages`):

```json
{
  "data": [],
  "pagination": { "page": 9, "limit": 10, "total_items": 25, "total_pages": 3, "has_next": false, "has_prev": true }
}
```

**Errors — `400 Bad Request`**

```sh
curl -i "http://localhost:3000/products?limit=500"
```
```json
{ "error": { "code": "BAD_REQUEST", "message": "`limit` must be between 1 and 100" } }
```

```sh
curl -i "http://localhost:3000/products?page=0"
```
```json
{ "error": { "code": "BAD_REQUEST", "message": "`page` must be >= 1" } }
```

```sh
curl -i "http://localhost:3000/products?page=abc"
```
```json
{ "error": { "code": "BAD_REQUEST", "message": "Failed to deserialize query string: page: invalid digit found in string" } }
```

```sh
curl -i "http://localhost:3000/products?sort=bogus"
```
```json
{ "error": { "code": "BAD_REQUEST", "message": "invalid sort `bogus`; allowed: created_at, price, name (prefix with `-` for descending)" } }
```

---

### 2. Create a product

`POST /products`

**Request body**

| Field      | Type   | Required |
|------------|--------|----------|
| `sku`      | string | yes (unique, not empty) |
| `name`     | string | yes (not empty)         |
| `price`    | number | yes (`>= 0`)            |
| `category` | string | no                      |

**curl**

```sh
curl -i -X POST http://localhost:3000/products \
  -H 'Content-Type: application/json' \
  -d '{"sku": "SKU-1", "name": "Keyboard", "price": 49.99, "category": "electronics"}'
```

**Response — `201 Created`**

```json
{
  "id": "6ae4364d9a2c8b0793e0b74d",
  "sku": "SKU-1",
  "name": "Keyboard",
  "price": 49.99,
  "category": "electronics",
  "created_at": "2026-10-30T06:16:45.500Z",
  "updated_at": "2026-10-30T06:16:45.500Z"
}
```

**Response — `409 Conflict`** (the `sku` already exists)

```sh
curl -i -X POST http://localhost:3000/products \
  -H 'Content-Type: application/json' \
  -d '{"sku": "SKU-1", "name": "Duplicate", "price": 1}'
```
```json
{ "error": { "code": "CONFLICT", "message": "a resource with the same unique key already exists" } }
```

**Response — `422 Unprocessable Entity`** (validation)

```sh
curl -i -X POST http://localhost:3000/products \
  -H 'Content-Type: application/json' \
  -d '{"sku": "SKU-X", "name": "Negative", "price": -1}'
```
```json
{ "error": { "code": "VALIDATION_ERROR", "message": "`price` must be a non-negative number" } }
```

```sh
curl -i -X POST http://localhost:3000/products \
  -H 'Content-Type: application/json' \
  -d '{"name": "No SKU", "price": 1}'
```
```json
{ "error": { "code": "VALIDATION_ERROR", "message": "Failed to deserialize the JSON body into the target type: missing field `sku` at line 1 column 26" } }
```

---

### 3. Get a product

`GET /products/{id}`

**curl**

```sh
curl -i http://localhost:3000/products/6ae4364d9a2c8b0793e0b765
```

**Response — `200 OK`**

```json
{
  "id": "6ae4364d9a2c8b0793e0b765",
  "sku": "SKU-25",
  "name": "Product 25",
  "price": 25.5,
  "category": "odd",
  "created_at": "2026-10-30T06:16:45.873Z",
  "updated_at": "2026-10-30T06:16:45.873Z"
}
```

**Response — `404 Not Found`**

```json
{ "error": { "code": "NOT_FOUND", "message": "product `6ae4364d9a2c8b0793e0b765` not found" } }
```

**Response — `400 Bad Request`** (not an ObjectId)

```sh
curl -i http://localhost:3000/products/nope
```
```json
{ "error": { "code": "BAD_REQUEST", "message": "`nope` is not a valid product id" } }
```

---

### 4. Update a product

`PUT /products/{id}`

Partial update: only the fields you send are changed. `sku` cannot be changed. At least one field is required.

**Request body**

| Field      | Type   | Required |
|------------|--------|----------|
| `name`     | string | no (not empty if sent) |
| `price`    | number | no (`>= 0` if sent)    |
| `category` | string | no                     |

**curl**

```sh
curl -i -X PUT http://localhost:3000/products/6ae4364d9a2c8b0793e0b765 \
  -H 'Content-Type: application/json' \
  -d '{"price": 99.9}'
```

**Response — `200 OK`** (the updated document)

```json
{
  "id": "6ae4364d9a2c8b0793e0b765",
  "sku": "SKU-25",
  "name": "Product 25",
  "price": 99.9,
  "category": "odd",
  "created_at": "2026-10-30T06:16:45.873Z",
  "updated_at": "2026-10-30T06:16:45.979Z"
}
```

**Response — `422 Unprocessable Entity`** (empty body)

```sh
curl -i -X PUT http://localhost:3000/products/6ae4364d9a2c8b0793e0b765 \
  -H 'Content-Type: application/json' \
  -d '{}'
```
```json
{ "error": { "code": "VALIDATION_ERROR", "message": "provide at least one of `name`, `price`, `category`" } }
```

**Response — `404 Not Found`** / **`400 Bad Request`**: same as [Get a product](#3-get-a-product).

---

### 5. Delete a product

`DELETE /products/{id}`

**curl**

```sh
curl -i -X DELETE http://localhost:3000/products/6ae4364d9a2c8b0793e0b765
```

**Response — `204 No Content`** — empty body.

**Response — `404 Not Found`**

```json
{ "error": { "code": "NOT_FOUND", "message": "product `6ae4364d9a2c8b0793e0b765` not found" } }
```

---

### When MongoDB is unavailable

Any `/products` request during an outage (after retries are exhausted):

**`503 Service Unavailable`** with header `Retry-After: 5`

```json
{ "error": { "code": "SERVICE_UNAVAILABLE", "message": "the database is temporarily unavailable" } }
```

If an attempt exceeds `MONGO_OP_TIMEOUT_MS` on every try:

**`504 Gateway Timeout`** with header `Retry-After: 5`

```json
{ "error": { "code": "TIMEOUT", "message": "the database did not respond in time" } }
```

---

## Items API (in memory)

### Data model

| Field         | Type             | Description                             |
|---------------|------------------|-----------------------------------------|
| `id`          | string (UUID v4) | Generated by the server on creation.    |
| `name`        | string           | Required on creation.                   |
| `description` | string \| null   | Optional.                               |

### Endpoints overview

| Method   | Path           | Description            | Success status   |
|----------|----------------|------------------------|------------------|
| `GET`    | `/items`       | List all items         | `200 OK`         |
| `POST`   | `/items`       | Create a new item      | `201 Created`    |
| `GET`    | `/items/{id}`  | Get one item by ID     | `200 OK`         |
| `PUT`    | `/items/{id}`  | Update an item         | `200 OK`         |
| `DELETE` | `/items/{id}`  | Delete an item         | `204 No Content` |

### 1. List all items

Order is **not guaranteed**.

```sh
curl -i http://localhost:3000/items
```

**`200 OK`**

```json
[
  { "id": "d5ede8cd-88fb-48a0-90d6-9b1b38af4a22", "name": "apple", "description": "A red fruit" },
  { "id": "8f14e45f-ceea-4e7a-9b1d-2c3f6a7b8c90", "name": "banana", "description": null }
]
```

### 2. Create an item

```sh
curl -i -X POST http://localhost:3000/items \
  -H 'Content-Type: application/json' \
  -d '{"name": "apple", "description": "A red fruit"}'
```

**`201 Created`**

```json
{ "id": "d5ede8cd-88fb-48a0-90d6-9b1b38af4a22", "name": "apple", "description": "A red fruit" }
```

### 3. Get an item

```sh
curl -i http://localhost:3000/items/d5ede8cd-88fb-48a0-90d6-9b1b38af4a22
```

**`200 OK`**

```json
{ "id": "d5ede8cd-88fb-48a0-90d6-9b1b38af4a22", "name": "apple", "description": "A red fruit" }
```

**`404 Not Found`** — empty body.

### 4. Update an item

Partial update. Sending `"description": null` is the same as omitting it, so a description cannot currently be cleared.

```sh
curl -i -X PUT http://localhost:3000/items/d5ede8cd-88fb-48a0-90d6-9b1b38af4a22 \
  -H 'Content-Type: application/json' \
  -d '{"name": "green apple", "description": "A sour fruit"}'
```

**`200 OK`**

```json
{ "id": "d5ede8cd-88fb-48a0-90d6-9b1b38af4a22", "name": "green apple", "description": "A sour fruit" }
```

**`404 Not Found`** — empty body.

### 5. Delete an item

```sh
curl -i -X DELETE http://localhost:3000/items/d5ede8cd-88fb-48a0-90d6-9b1b38af4a22
```

**`204 No Content`** — empty body. **`404 Not Found`** — empty body.

---

## Error responses

### `/products` — JSON errors

All errors have the same shape:

```json
{ "error": { "code": "<CODE>", "message": "<human readable message>" } }
```

| Status                       | `code`                | When                                                         |
|------------------------------|-----------------------|--------------------------------------------------------------|
| `400 Bad Request`            | `BAD_REQUEST`         | Invalid id, invalid query params, malformed JSON, missing `Content-Type`. |
| `404 Not Found`              | `NOT_FOUND`           | No product with that id.                                     |
| `409 Conflict`               | `CONFLICT`            | Duplicate `sku`.                                             |
| `429 Too Many Requests`      | `RATE_LIMITED`        | Rate limit exceeded (any route). Includes `Retry-After`.     |
| `422 Unprocessable Entity`   | `VALIDATION_ERROR`    | Valid JSON with missing/invalid fields.                      |
| `500 Internal Server Error`  | `INTERNAL_ERROR`      | Unexpected error (details only in server logs).              |
| `503 Service Unavailable`    | `SERVICE_UNAVAILABLE` | MongoDB unreachable after retries. Includes `Retry-After`.   |
| `504 Gateway Timeout`        | `TIMEOUT`             | MongoDB did not answer in time after retries. Includes `Retry-After`. |

```sh
# 400 — malformed JSON
curl -i -X POST http://localhost:3000/products \
  -H 'Content-Type: application/json' \
  -d '{bad'

# 400 — missing Content-Type header
curl -i -X POST http://localhost:3000/products -d '{"sku":"A","name":"a","price":1}'
```

### `/items` — plain text errors

Generated by the framework; the body is plain text (or empty).

| Status                       | When                                              | Example body |
|------------------------------|---------------------------------------------------|--------------|
| `400 Bad Request`            | `{id}` is not a valid UUID                        | ``Invalid URL: Cannot parse `id` with value `not-a-uuid`: UUID parsing failed: invalid character: found `n` at 0`` |
| `400 Bad Request`            | Body is not valid JSON                            | `Failed to parse the request body as JSON: key must be a string at line 1 column 2` |
| `404 Not Found`              | No item with the given ID                         | *(empty)* |
| `415 Unsupported Media Type` | Missing `Content-Type: application/json` header   | ``Expected request with `Content-Type: application/json` `` |
| `422 Unprocessable Entity`   | Valid JSON but missing `name`                     | ``Failed to deserialize the JSON body into the target type: missing field `name` at line 1 column 19`` |

```sh
curl -i http://localhost:3000/items/not-a-uuid
curl -i http://localhost:3000/items/00000000-0000-0000-0000-000000000000
curl -i -X POST http://localhost:3000/items -d '{"name": "apple"}'
curl -i -X POST http://localhost:3000/items -H 'Content-Type: application/json' -d '{"description": "no name"}'
```

### Any route

`405 Method Not Allowed` (empty body) for an unsupported method, e.g. `curl -i -X PATCH http://localhost:3000/items`.

---

## End-to-end walkthroughs

### Products: seed data and paginate

```sh
BASE=http://localhost:3000

# Seed 25 products in two categories
for i in $(seq 1 25); do
  CAT=$([ $((i % 2)) -eq 0 ] && echo even || echo odd)
  curl -s -o /dev/null -w "%{http_code} " -X POST $BASE/products \
    -H 'Content-Type: application/json' \
    -d "{\"sku\":\"SKU-$i\",\"name\":\"Product $i\",\"price\":$i.5,\"category\":\"$CAT\"}"
done; echo

# Walk the pages
curl -s "$BASE/products?limit=10&page=1&sort=price"; echo
curl -s "$BASE/products?limit=10&page=2&sort=price"; echo
curl -s "$BASE/products?limit=10&page=3&sort=price"; echo

# Filter
curl -s "$BASE/products?category=even&limit=5"; echo
```

### Products: full CRUD

```sh
BASE=http://localhost:3000

ID=$(curl -s -X POST $BASE/products \
  -H 'Content-Type: application/json' \
  -d '{"sku":"DEMO-1","name":"Demo","price":10}' | sed -E 's/.*"id":"([^"]+)".*/\1/')
echo "Created product $ID"

curl -s $BASE/products/$ID; echo
curl -s -X PUT $BASE/products/$ID -H 'Content-Type: application/json' -d '{"price": 12.5}'; echo
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE $BASE/products/$ID   # 204
curl -s -o /dev/null -w '%{http_code}\n' $BASE/products/$ID             # 404
```

### Products: simulate an outage

```sh
docker stop catalog-mongo                        # or: podman stop catalog-mongo
curl -i http://localhost:3000/health/ready        # 503 degraded
curl -i http://localhost:3000/products            # 503 after retries (see logs)
curl -i http://localhost:3000/items               # still 200
docker start catalog-mongo                       # or: podman start catalog-mongo
sleep 3
curl -i http://localhost:3000/products            # 200 again, no restart needed
```

### Items: full CRUD

```sh
BASE=http://localhost:3000

ID=$(curl -s -X POST $BASE/items \
  -H 'Content-Type: application/json' \
  -d '{"name": "apple"}' | sed -E 's/.*"id":"([^"]+)".*/\1/')
echo "Created item $ID"

curl -s $BASE/items; echo
curl -s $BASE/items/$ID; echo
curl -s -X PUT $BASE/items/$ID -H 'Content-Type: application/json' -d '{"description": "A red fruit"}'; echo
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE $BASE/items/$ID   # 204
curl -s -o /dev/null -w '%{http_code}\n' $BASE/items/$ID             # 404
```

If you have [`jq`](https://jqlang.github.io/jq/) installed, extract IDs more robustly with `| jq -r .id` instead of `sed`.
