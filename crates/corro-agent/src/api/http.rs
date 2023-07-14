use std::{
    collections::HashMap,
    ops::RangeInclusive,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{response::IntoResponse, Extension};
use bytes::{BufMut, BytesMut};
use compact_str::{format_compact, ToCompactString};
use corro_types::{
    agent::{Agent, ChangeError, KnownDbVersion},
    api::{RowResult, RqliteResponse, RqliteResult, Statement},
    broadcast::{ChangeV1, Changeset, Timestamp},
    change::{row_to_change, SqliteValue},
    pubsub::{ChangeType, Matcher, MatcherCmd},
    schema::{make_schema_inner, parse_sql},
    sqlite::SqlitePoolError,
};
use futures::future::poll_fn;
use hyper::StatusCode;
use rusqlite::{params, params_from_iter, ToSql, Transaction};
use spawn::spawn_counted;
use tokio::{
    sync::{
        broadcast,
        mpsc::{self, channel},
        oneshot, RwLock as TokioRwLock,
    },
    task::block_in_place,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use corro_types::{
    broadcast::{BroadcastInput, Message, MessageV1},
    change::Change,
};
use uuid::Uuid;

use crate::agent::process_subs;

pub const MAX_CHANGES_PER_MESSAGE: usize = 50;

// TODO: accept a few options
// #[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
// #[serde(rename_all = "snake_case")]
// pub struct RqliteRequestOptions {
//     pretty: Option<bool>,
//     timings: Option<bool>,
//     transaction: Option<bool>,
//     q: Option<String>,
// }

pub struct ChunkedChanges<I> {
    iter: I,
    changes: Vec<Change>,
    last_pushed_seq: i64,
    last_start_seq: i64,
    last_seq: i64,
    chunk_size: usize,
    done: bool,
}

impl<I> ChunkedChanges<I> {
    pub fn new(iter: I, start_seq: i64, last_seq: i64, chunk_size: usize) -> Self {
        Self {
            iter,
            changes: vec![],
            last_pushed_seq: 0,
            last_start_seq: start_seq,
            last_seq,
            chunk_size,
            done: false,
        }
    }
}

impl<'stmt, I> Iterator for ChunkedChanges<I>
where
    I: Iterator<Item = rusqlite::Result<Change>>,
{
    type Item = Result<(Vec<Change>, RangeInclusive<i64>), rusqlite::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        // previously marked as done because the Rows iterator returned None
        if self.done {
            return None;
        }

        loop {
            trace!("chunking through the rows iterator");
            match self.iter.next() {
                Some(Ok(change)) => {
                    trace!("got change: {change:?}");
                    self.last_pushed_seq = change.seq;
                    self.changes.push(change);

                    if self.last_pushed_seq == self.last_seq {
                        // this was the last seq! break early
                        self.done = true;
                        break;
                    }

                    if self.changes.len() >= self.chunk_size {
                        // chunking it up
                        let start_seq = self.last_start_seq;
                        self.last_start_seq = self.last_pushed_seq + 1;
                        return Some(Ok((
                            self.changes.drain(..).collect(),
                            start_seq..=self.last_pushed_seq,
                        )));
                    }
                }
                None => {
                    // marking as done for the next `next()` call
                    self.done = true;
                    // break out of the loop, don't return, there might be buffered changes
                    break;
                }
                Some(Err(e)) => return Some(Err(e)),
            }
        }

        // return buffered changes
        return Some(Ok((
            self.changes.clone(),                // no need to drain here like before
            self.last_start_seq..=self.last_seq, // even if empty, this is all we have still applied
        )));
    }
}

pub async fn make_broadcastable_changes<F, T>(
    agent: &Agent,
    f: F,
) -> Result<(T, Duration), ChangeError>
where
    F: Fn(&Transaction) -> Result<T, ChangeError>,
{
    trace!("getting conn...");
    let mut conn = agent.pool().write_priority().await?;
    trace!("got conn");

    let actor_id = agent.actor_id();

    let start = Instant::now();
    block_in_place(move || {
        let tx = conn.transaction()?;

        // Execute whatever might mutate state data
        let ret = f(&tx)?;

        let ts = Timestamp::from(agent.clock().new_timestamp());

        let db_version: i64 = tx
            .prepare_cached("SELECT crsql_nextdbversion()")?
            .query_row((), |row| row.get(0))?;

        let has_changes: bool = tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM crsql_changes WHERE site_id IS NULL AND db_version = ?);",
        )?
        .query_row([db_version], |row| row.get(0))?;

        let last_seq: Option<i64> = if has_changes {
            tx.prepare_cached(
                "SELECT MAX(seq) FROM crsql_changes WHERE site_id IS NULL AND db_version = ?",
            )?
            .query_row([db_version], |row| row.get(0))?
        } else {
            None
        };

        let booked = agent.bookie().for_actor(actor_id);
        // maybe we should do this earlier, but there can only ever be 1 write conn at a time,
        // so it probably doesn't matter too much, except for reads of internal state
        let mut book_writer = booked.write();

        let last_version = book_writer.last().unwrap_or(0);
        trace!("last_version: {last_version}");
        let version = last_version + 1;
        trace!("version: {version}");

        let elapsed = {
            if let Some(last_seq) = last_seq {
                tx.prepare_cached(
                    r#"
                INSERT INTO __corro_bookkeeping (actor_id, start_version, db_version, last_seq, ts)
                    VALUES (?, ?, ?, ?, ?);
            "#,
                )?
                .execute(params![actor_id, version, db_version, last_seq, ts])?;
            }

            tx.commit()?;
            start.elapsed()
        };

        trace!("committed tx, db_version: {db_version}, last_seq: {last_seq:?}");

        if let Some(last_seq) = last_seq {
            book_writer.insert(
                version,
                KnownDbVersion::Current {
                    db_version,
                    last_seq,
                    ts,
                },
            );

            let agent = agent.clone();

            spawn_counted(async move {
                let conn = agent.pool().read().await?;

                block_in_place(|| {
                    let mut prepped = conn.prepare_cached(r#"SELECT "table", pk, cid, val, col_version, db_version, seq, COALESCE(site_id, crsql_siteid()) FROM crsql_changes WHERE site_id IS NULL AND db_version = ? ORDER BY seq ASC"#)?;
                    let rows = prepped.query_map([db_version], row_to_change)?;
                    let mut chunked =
                        ChunkedChanges::new(rows, 0, last_seq, MAX_CHANGES_PER_MESSAGE);
                    while let Some(changes_seqs) = chunked.next() {
                        match changes_seqs {
                            Ok((changes, seqs)) => {
                                process_subs(&agent, &changes, db_version);

                                trace!("broadcasting changes: {changes:?} for seq: {seqs:?}");

                                let tx_bcast = agent.tx_bcast().clone();
                                tokio::spawn(async move {
                                    if let Err(e) = tx_bcast
                                        .send(BroadcastInput::AddBroadcast(Message::V1(
                                            MessageV1::Change(ChangeV1 {
                                                actor_id,
                                                changeset: Changeset::Full {
                                                    version,
                                                    changes,
                                                    seqs,
                                                    last_seq,
                                                    ts,
                                                },
                                            }),
                                        )))
                                        .await
                                    {
                                        error!("could not send change message for broadcast: {e}");
                                    }
                                });
                            }
                            Err(e) => {
                                error!("could not process crsql change (db_version: {db_version}) for broadcast: {e}");
                                break;
                            }
                        }
                    }
                    Ok::<_, rusqlite::Error>(())
                })?;

                Ok::<_, eyre::Report>(())
            });
        }

        Ok::<_, ChangeError>((ret, elapsed))
    })
}

fn execute_statement(tx: &Transaction, stmt: &Statement) -> rusqlite::Result<usize> {
    match stmt {
        Statement::Simple(q) => tx.execute(&q, []),
        Statement::WithParams(q, params) => tx.execute(&q, params_from_iter(params.into_iter())),
        Statement::WithNamedParams(q, params) => tx.execute(
            &q,
            params
                .iter()
                .map(|(k, v)| (k.as_str(), v as &dyn ToSql))
                .collect::<Vec<(&str, &dyn ToSql)>>()
                .as_slice(),
        ),
    }
}

pub async fn api_v1_transactions(
    // axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Extension(agent): Extension<Agent>,
    axum::extract::Json(statements): axum::extract::Json<Vec<Statement>>,
) -> (StatusCode, axum::Json<RqliteResponse>) {
    if statements.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: "at least 1 statement is required".into(),
                }],
                time: None,
            }),
        );
    }

    let res = make_broadcastable_changes(&agent, move |tx| {
        let mut total_rows_affected = 0;

        let results = statements
            .iter()
            .filter_map(|stmt| {
                let start = Instant::now();
                let res = execute_statement(&tx, stmt);

                Some(match res {
                    Ok(rows_affected) => {
                        total_rows_affected += rows_affected;
                        RqliteResult::Execute {
                            rows_affected,
                            time: Some(start.elapsed().as_secs_f64()),
                        }
                    }
                    Err(e) => RqliteResult::Error {
                        error: e.to_string(),
                    },
                })
            })
            .collect::<Vec<RqliteResult>>();

        Ok(results)
    })
    .await;

    let (results, elapsed) = match res {
        Ok(res) => res,
        Err(e) => match e {
            e => {
                error!("could not execute statement(s): {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(RqliteResponse {
                        results: vec![RqliteResult::Error {
                            error: e.to_string(),
                        }],
                        time: None,
                    }),
                );
            }
        },
    };

    (
        StatusCode::OK,
        axum::Json(RqliteResponse {
            results,
            time: Some(elapsed.as_secs_f64()),
        }),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("pool connection acquisition error")]
    Pool(#[from] SqlitePoolError),
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),
}

async fn build_query_rows_response(
    agent: &Agent,
    data_tx: mpsc::Sender<RowResult>,
    stmt: Statement,
) -> Option<(StatusCode, RqliteResult)> {
    let (res_tx, res_rx) = oneshot::channel();

    let pool = agent.pool().clone();

    tokio::spawn(async move {
        let conn = match pool.read().await {
            Ok(conn) => conn,
            Err(e) => {
                _ = res_tx.send(Some((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    RqliteResult::Error {
                        error: e.to_string(),
                    },
                )));
                return;
            }
        };

        let prepped_res = block_in_place(|| match stmt {
            Statement::Simple(q) => conn.prepare(q.as_str()),
            Statement::WithParams(q, _) => conn.prepare(q.as_str()),
            Statement::WithNamedParams(q, _) => conn.prepare(q.as_str()),
        });

        let mut prepped = match prepped_res {
            Ok(prepped) => prepped,
            Err(e) => {
                _ = res_tx.send(Some((
                    StatusCode::BAD_REQUEST,
                    RqliteResult::Error {
                        error: e.to_string(),
                    },
                )));
                return;
            }
        };

        block_in_place(|| {
            let col_count = prepped.column_count();

            if let Err(e) = data_tx.blocking_send(RowResult::Columns(
                prepped
                    .columns()
                    .into_iter()
                    .map(|col| col.name().to_compact_string())
                    .collect(),
            )) {
                error!("could not send back columns: {e}");
                return;
            }

            let mut rows = match prepped.query(()) {
                Ok(rows) => rows,
                Err(e) => {
                    _ = res_tx.send(Some((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        RqliteResult::Error {
                            error: e.to_string(),
                        },
                    )));
                    return;
                }
            };

            if let Err(_e) = res_tx.send(None) {
                error!("could not send back response through oneshot channel, aborting");
                return;
            }

            let mut rowid = 1;

            loop {
                match rows.next() {
                    Ok(Some(row)) => {
                        match (0..col_count)
                            .map(|i| row.get::<_, SqliteValue>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                        {
                            Ok(cells) => {
                                if let Err(e) = data_tx.blocking_send(RowResult::Row {
                                    change_type: ChangeType::Upsert,
                                    rowid,
                                    cells,
                                }) {
                                    error!("could not send back row: {e}");
                                    return;
                                }
                                rowid += 1;
                            }
                            Err(e) => {
                                _ = data_tx.blocking_send(RowResult::Error(e.to_compact_string()));
                                return;
                            }
                        }
                    }
                    Ok(None) => {
                        // done!
                        break;
                    }
                    Err(e) => {
                        _ = data_tx.blocking_send(RowResult::Error(e.to_compact_string()));
                        return;
                    }
                }
            }
        });
    });

    match res_rx.await {
        Ok(res) => res,
        Err(e) => Some((
            StatusCode::INTERNAL_SERVER_ERROR,
            RqliteResult::Error {
                error: e.to_string(),
            },
        )),
    }
}

pub async fn api_v1_watch_by_id(
    Extension(agent): Extension<Agent>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> impl IntoResponse {
    watch_by_id(agent, id).await
}

async fn watch_by_id(agent: Agent, id: Uuid) -> hyper::Response<hyper::Body> {
    let matcher = match { agent.matchers().read().get(&id).cloned() } {
        Some(matcher) => matcher,
        None => {
            return hyper::Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(
                    serde_json::to_vec(&RowResult::Error(format_compact!(
                        "could not find watcher with id {id}"
                    )))
                    .expect("could not serialize queries stream error")
                    .into(),
                )
                .expect("could not build error response")
        }
    };

    let (tx, body) = hyper::Body::channel();

    // TODO: timeout on data send instead of infinitely waiting for channel space.
    let (init_tx, init_rx) = channel(512);
    let change_rx = matcher.subscribe();
    let cancel = matcher.cancel();

    tokio::spawn(process_watch_channel(
        agent.clone(),
        id,
        tx,
        init_rx,
        change_rx,
        matcher.cmd_tx().clone(),
        cancel,
    ));

    let pool = agent.pool().dedicated_pool().clone();
    tokio::spawn(async move {
        if let Err(_e) = init_tx
            .send(RowResult::Columns(matcher.0.col_names.clone()))
            .await
        {
            warn!("could not send back column names, client is probably gone. returning.");
            return;
        }

        let conn = match pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                _ = init_tx.send(RowResult::Error(e.to_compact_string())).await;
                return;
            }
        };

        #[derive(Debug, thiserror::Error)]
        enum QueryTempError {
            #[error(transparent)]
            Sqlite(#[from] rusqlite::Error),
            #[error(transparent)]
            Send(#[from] mpsc::error::SendError<RowResult>),
        }

        let res = block_in_place(|| {
            let mut query_cols = vec![];
            for i in 0..(matcher.0.parsed.columns.len()) {
                query_cols.push(format!("col_{i}"));
            }
            let mut prepped = conn.prepare_cached(&format!(
                "SELECT __corro_rowid,{} FROM {}",
                query_cols.join(","),
                matcher.table_name()
            ))?;
            let col_count = prepped.column_count();

            init_tx.blocking_send(RowResult::Columns(matcher.0.col_names.clone()))?;

            let mut rows = prepped.query(())?;

            loop {
                let row = match rows.next()? {
                    Some(row) => row,
                    None => break,
                };
                let rowid = row.get(0)?;

                let cells = (1..col_count)
                    .map(|i| row.get::<_, SqliteValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                init_tx.blocking_send(RowResult::Row {
                    rowid,
                    change_type: ChangeType::Upsert,
                    cells,
                })?;
            }

            Ok::<_, QueryTempError>(())
        });

        if let Err(QueryTempError::Sqlite(e)) = res {
            _ = init_tx.send(RowResult::Error(e.to_compact_string())).await;
        }

        _ = init_tx.send(RowResult::EndOfQuery).await;
    });

    hyper::Response::builder()
        .status(StatusCode::OK)
        .header("corro-query-id", id.to_string())
        .body(body)
        .expect("could not build query response body")
}

async fn process_watch_channel(
    agent: Agent,
    matcher_id: Uuid,
    mut tx: hyper::body::Sender,
    mut init_rx: mpsc::Receiver<RowResult>,
    mut change_rx: broadcast::Receiver<RowResult>,
    cmd_tx: mpsc::Sender<MatcherCmd>,
    cancel: CancellationToken,
) {
    let mut buf = BytesMut::new();

    let mut init_done = false;
    let mut check_ready = interval(Duration::from_secs(1));
    let mut cancelled = false;
    loop {
        // either we get data we need to transmit
        // or we check every 1s if the client is still ready to receive data
        let row_res = tokio::select! {
            _ = cancel.cancelled() => {
                debug!("canceled!");
                cancelled = true;
                break;
            },
            maybe_row_res = init_rx.recv(), if !init_done => match maybe_row_res {
                Some(row_res) => row_res,
                None => {
                    init_done = true;
                    continue;
                }
            },
            res = change_rx.recv(), if init_done => match res {
                Ok(row_res) => row_res,
                Err(e) => {
                    warn!("could not receive change: {e}");
                    break;
                }
            },
            _ = check_ready.tick() => {
                if let Err(_) = poll_fn(|cx| tx.poll_ready(cx)).await {
                    break;
                }
                continue;
            }
        };

        if matches!(row_res, RowResult::EndOfQuery) {
            init_done = true;
        }

        {
            let mut writer = (&mut buf).writer();
            if let Err(e) = serde_json::to_writer(&mut writer, &row_res) {
                _ = tx
                    .send_data(
                        serde_json::to_vec(&serde_json::json!(RowResult::Error(
                            e.to_compact_string()
                        )))
                        .expect("could not serialize error json")
                        .into(),
                    )
                    .await;
                return;
            }
        }

        buf.extend_from_slice(b"\n");

        if let Err(e) = tx.send_data(buf.split().freeze()).await {
            error!("could not send data through body's channel: {e}");
            return;
        }
    }
    debug!("query body channel done");

    if cancelled {
        // try to remove if it exists.
        agent.matchers().write().remove(&matcher_id);
    } else {
        _ = cmd_tx.send(MatcherCmd::Unsubscribe).await;
    }
}

pub type MatcherCache = Arc<TokioRwLock<HashMap<String, Uuid>>>;

pub async fn api_v1_watches(
    Extension(agent): Extension<Agent>,
    Extension(watch_cache): Extension<MatcherCache>,
    axum::extract::Json(stmt): axum::extract::Json<Statement>,
) -> impl IntoResponse {
    let stmt = match stmt {
        Statement::Simple(s) => s,
        _ => {
            return hyper::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(
                    serde_json::to_vec(&RowResult::Error(
                        "only simple statements (no params) are accepted for watches".into(),
                    ))
                    .expect("could not serialize queries stream error")
                    .into(),
                )
                .expect("could not build error response");
        }
    };

    if let Some(matcher_id) = { watch_cache.read().await.get(&stmt).cloned() } {
        let contains = { agent.matchers().read().contains_key(&matcher_id) };
        if contains {
            info!("reusing matcher id {matcher_id}");
            return watch_by_id(agent, matcher_id).await;
        }
    }

    let (tx, body) = hyper::Body::channel();

    // TODO: timeout on data send instead of infinitely waiting for channel space.
    let (data_tx, data_rx) = channel(512);
    let (change_tx, change_rx) = broadcast::channel(128);

    let matcher_id = Uuid::new_v4();
    let cancel = CancellationToken::new();

    let conn = match agent.pool().dedicated().await {
        Ok(conn) => conn,
        Err(e) => {
            return hyper::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(
                    serde_json::to_vec(&RowResult::Error(e.to_compact_string()))
                        .expect("could not serialize queries stream error")
                        .into(),
                )
                .expect("could not build error response")
        }
    };

    let matcher = match block_in_place(|| {
        Matcher::new(
            matcher_id,
            &agent.schema().read(),
            conn,
            data_tx.clone(),
            change_tx,
            &stmt,
            cancel.clone(),
        )
    }) {
        Ok(m) => m,
        Err(e) => {
            return hyper::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(
                    serde_json::to_vec(&RowResult::Error(e.to_compact_string()))
                        .expect("could not serialize queries stream error")
                        .into(),
                )
                .expect("could not build error response")
        }
    };

    {
        agent.matchers().write().insert(matcher_id, matcher.clone());
    }

    tokio::spawn(process_watch_channel(
        agent.clone(),
        matcher_id,
        tx,
        data_rx,
        change_rx,
        matcher.cmd_tx().clone(),
        cancel.clone(),
    ));

    hyper::Response::builder()
        .status(StatusCode::OK)
        .header("corro-query-id", matcher_id.to_string())
        .body(body)
        .expect("could not generate ok http response for query request")
}

pub async fn api_v1_queries(
    Extension(agent): Extension<Agent>,
    axum::extract::Json(stmt): axum::extract::Json<Statement>,
) -> impl IntoResponse {
    let (mut tx, body) = hyper::Body::channel();

    // TODO: timeout on data send instead of infinitely waiting for channel space.
    let (data_tx, mut data_rx) = channel(512);

    tokio::spawn(async move {
        let mut buf = BytesMut::new();

        while let Some(row_res) = data_rx.recv().await {
            {
                let mut writer = (&mut buf).writer();
                if let Err(e) = serde_json::to_writer(&mut writer, &row_res) {
                    _ = tx
                        .send_data(
                            serde_json::to_vec(&serde_json::json!(RowResult::Error(
                                e.to_compact_string()
                            )))
                            .expect("could not serialize error json")
                            .into(),
                        )
                        .await;
                    return;
                }
            }

            buf.extend_from_slice(b"\n");

            if let Err(e) = tx.send_data(buf.split().freeze()).await {
                error!("could not send data through body's channel: {e}");
                return;
            }
        }
        debug!("query body channel done");
    });

    match build_query_rows_response(&agent, data_tx, stmt).await {
        Some((status, res)) => {
            return hyper::Response::builder()
                .status(status)
                .body(
                    serde_json::to_vec(&res)
                        .expect("could not serialize query error response")
                        .into(),
                )
                .expect("could not build query response body");
        }
        None => {
            return hyper::Response::builder()
                .status(StatusCode::OK)
                .body(body)
                .expect("could not build query response body");
        }
    }
}

async fn execute_schema(agent: &Agent, statements: Vec<String>) -> eyre::Result<()> {
    let new_sql: String = statements.join(";");

    let partial_schema = parse_sql(&new_sql)?;

    let mut conn = agent.pool().write_priority().await?;

    // hold onto this lock so nothing else makes changes
    let mut schema_write = agent.schema().write();

    // clone the previous schema and apply
    let new_schema = {
        let mut schema = schema_write.clone();
        for (name, def) in partial_schema.tables.iter() {
            // overwrite table because users are expected to return a full table def
            schema.tables.insert(name.clone(), def.clone());
        }
        schema
    };

    block_in_place(|| {
        let tx = conn.transaction()?;

        make_schema_inner(&tx, &schema_write, &new_schema)?;

        for tbl_name in partial_schema.tables.keys() {
            tx.execute("DELETE FROM __corro_schema WHERE tbl_name = ?", [tbl_name])?;

            let n = tx.execute("INSERT INTO __corro_schema SELECT tbl_name, type, name, sql, 'api' AS source FROM sqlite_schema WHERE tbl_name = ? AND type IN ('table', 'index') AND name IS NOT NULL AND sql IS NOT NULL", [tbl_name])?;
            info!("updated {n} rows in __corro_schema for table {tbl_name}");
        }

        tx.commit()?;

        Ok::<_, eyre::Report>(())
    })?;

    *schema_write = new_schema;

    Ok(())
}

pub async fn api_v1_db_schema(
    Extension(agent): Extension<Agent>,
    axum::extract::Json(statements): axum::extract::Json<Vec<String>>,
) -> (StatusCode, axum::Json<RqliteResponse>) {
    if statements.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: "at least 1 statement is required".into(),
                }],
                time: None,
            }),
        );
    }

    let start = Instant::now();

    if let Err(e) = execute_schema(&agent, statements).await {
        error!("could not merge schemas: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: e.to_string(),
                }],
                time: None,
            }),
        );
    }

    (
        StatusCode::OK,
        axum::Json(RqliteResponse {
            results: vec![],
            time: Some(start.elapsed().as_secs_f64()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;
    use bytes::Bytes;
    use corro_types::{actor::ActorId, agent::SplitPool, config::Config, schema::SqliteType};
    use futures::Stream;
    use http_body::{combinators::UnsyncBoxBody, Body};
    use tokio::sync::mpsc::{channel, error::TryRecvError};
    use tokio_util::codec::{Decoder, LinesCodec};
    use tripwire::Tripwire;
    use uuid::Uuid;

    use super::*;

    use crate::agent::migrate;

    struct UnsyncBodyStream(std::pin::Pin<Box<UnsyncBoxBody<Bytes, axum::Error>>>);

    impl Stream for UnsyncBodyStream {
        type Item = Result<Bytes, axum::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.as_mut().poll_data(cx)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_api_db_execute() -> eyre::Result<()> {
        _ = tracing_subscriber::fmt::try_init();

        let (tripwire, _tripwire_worker, _tripwire_tx) = Tripwire::new_simple();

        let dir = tempfile::tempdir()?;

        let pool = SplitPool::create(dir.path().join("./test.sqlite"), tripwire.clone()).await?;

        {
            let mut conn = pool.write_priority().await?;
            migrate(&mut conn)?;
        }

        let (tx_bcast, mut rx_bcast) = channel(1);
        let (tx_apply, _rx_apply) = channel(1);

        let agent = Agent::new(corro_types::agent::AgentConfig {
            actor_id: ActorId(Uuid::new_v4()),
            pool,
            config: ArcSwap::from_pointee(
                Config::builder()
                    .db_path(dir.path().join("corrosion.db").display().to_string())
                    .gossip_addr("127.0.0.1:1234".parse()?)
                    .api_addr("127.0.0.1:8080".parse()?)
                    .build()?,
            ),
            gossip_addr: "127.0.0.1:0".parse().unwrap(),
            api_addr: "127.0.0.1:0".parse().unwrap(),
            members: Default::default(),
            clock: Default::default(),
            bookie: Default::default(),
            subscribers: Default::default(),
            tx_bcast,
            tx_apply,
            schema: Default::default(),
            tripwire,
        });

        let (status_code, _body) = api_v1_db_schema(
            Extension(agent.clone()),
            axum::Json(vec![corro_tests::TEST_SCHEMA.into()]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        let (status_code, body) = api_v1_transactions(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(
                "insert into tests (id, text) values (?,?)".into(),
                vec!["service-id".into(), "service-name".into()],
            )]),
        )
        .await;

        println!("{body:?}");

        assert_eq!(status_code, StatusCode::OK);

        assert!(body.0.results.len() == 1);

        let msg = rx_bcast
            .recv()
            .await
            .expect("not msg received on bcast channel");

        assert!(matches!(
            msg,
            BroadcastInput::AddBroadcast(Message::V1(MessageV1::Change(ChangeV1 {
                changeset: Changeset::Full { version: 1, .. },
                ..
            })))
        ));

        assert_eq!(agent.bookie().last(&agent.actor_id()), Some(1));

        println!("second req...");

        let (status_code, body) = api_v1_transactions(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(
                "update tests SET text = ? where id = ?".into(),
                vec!["service-name".into(), "service-id".into()],
            )]),
        )
        .await;

        println!("{body:?}");

        assert_eq!(status_code, StatusCode::OK);

        assert!(body.0.results.len() == 1);

        // no actual changes!
        assert!(matches!(rx_bcast.try_recv(), Err(TryRecvError::Empty)));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_api_db_query() -> eyre::Result<()> {
        _ = tracing_subscriber::fmt::try_init();

        let (tripwire, _tripwire_worker, _tripwire_tx) = Tripwire::new_simple();

        let dir = tempfile::tempdir()?;

        let pool = SplitPool::create(dir.path().join("./test.sqlite"), tripwire.clone()).await?;

        {
            let mut conn = pool.write_priority().await?;
            migrate(&mut conn)?;
        }

        let (tx_bcast, _rx_bcast) = channel(1);
        let (tx_apply, _rx_apply) = channel(1);

        let agent = Agent::new(corro_types::agent::AgentConfig {
            actor_id: ActorId(Uuid::new_v4()),
            pool,
            config: ArcSwap::from_pointee(
                Config::builder()
                    .db_path(dir.path().join("corrosion.db").display().to_string())
                    .gossip_addr("127.0.0.1:1234".parse()?)
                    .api_addr("127.0.0.1:8080".parse()?)
                    .build()?,
            ),
            gossip_addr: "127.0.0.1:0".parse().unwrap(),
            api_addr: "127.0.0.1:0".parse().unwrap(),
            members: Default::default(),
            clock: Default::default(),
            bookie: Default::default(),
            subscribers: Default::default(),
            tx_bcast,
            tx_apply,
            schema: Default::default(),
            tripwire,
        });

        let (status_code, _body) = api_v1_db_schema(
            Extension(agent.clone()),
            axum::Json(vec![corro_tests::TEST_SCHEMA.into()]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        let (status_code, body) = api_v1_transactions(
            Extension(agent.clone()),
            axum::Json(vec![
                Statement::WithParams(
                    "insert into tests (id, text) values (?,?)".into(),
                    vec!["service-id".into(), "service-name".into()],
                ),
                Statement::WithParams(
                    "insert into tests (id, text) values (?,?)".into(),
                    vec!["service-id-2".into(), "service-name-2".into()],
                ),
            ]),
        )
        .await;

        // println!("{body:?}");

        assert_eq!(status_code, StatusCode::OK);

        assert!(body.0.results.len() == 2);

        let res = api_v1_queries(
            Extension(agent.clone()),
            axum::Json(Statement::Simple("select * from tests".into())),
        )
        .await
        .into_response();

        assert_eq!(res.status(), StatusCode::OK);

        let mut body = res.into_body();

        let mut lines = LinesCodec::new();

        let mut buf = BytesMut::new();

        buf.extend_from_slice(&body.data().await.unwrap()?);

        let s = lines.decode(&mut buf).unwrap().unwrap();

        let cols: RowResult = serde_json::from_str(&s).unwrap();

        assert_eq!(cols, RowResult::Columns(vec!["id".into(), "text".into()]));

        buf.extend_from_slice(&body.data().await.unwrap()?);

        let s = lines.decode(&mut buf).unwrap().unwrap();

        let row: RowResult = serde_json::from_str(&s).unwrap();

        assert_eq!(
            row,
            RowResult::Row {
                rowid: 1,
                change_type: ChangeType::Upsert,
                cells: vec!["service-id".into(), "service-name".into()]
            }
        );

        buf.extend_from_slice(&body.data().await.unwrap()?);

        let s = lines.decode(&mut buf).unwrap().unwrap();

        let row: RowResult = serde_json::from_str(&s).unwrap();

        assert_eq!(
            row,
            RowResult::Row {
                rowid: 2,
                change_type: ChangeType::Upsert,
                cells: vec!["service-id-2".into(), "service-name-2".into()]
            }
        );

        assert!(body.data().await.is_none());

        let res = api_v1_watches(
            Extension(agent.clone()),
            Extension(Default::default()),
            axum::Json(Statement::Simple("select * from tests".into())),
        )
        .await
        .into_response();

        assert_eq!(res.status(), StatusCode::OK);

        let mut body = res.into_body();

        let mut lines = LinesCodec::new();

        let mut buf = BytesMut::new();

        let (status_code, _) = api_v1_transactions(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(
                "insert into tests (id, text) values (?,?)".into(),
                vec!["service-id-3".into(), "service-name-3".into()],
            )]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "{\"columns\":[\"id\",\"text\"]}");

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "{\"row\":{\"rowid\":1,\"change_type\":\"upsert\",\"cells\":[\"service-id\",\"service-name\"]}}");

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "{\"row\":{\"rowid\":2,\"change_type\":\"upsert\",\"cells\":[\"service-id-2\",\"service-name-2\"]}}");

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "\"end_of_query\"");

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "{\"row\":{\"rowid\":3,\"change_type\":\"upsert\",\"cells\":[\"service-id-3\",\"service-name-3\"]}}");

        let (status_code, _) = api_v1_transactions(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(
                "insert into tests (id, text) values (?,?)".into(),
                vec!["service-id-4".into(), "service-name-4".into()],
            )]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        buf.extend_from_slice(&body.data().await.unwrap()?);
        let s = lines.decode(&mut buf).unwrap().unwrap();

        assert_eq!(s, "{\"row\":{\"rowid\":4,\"change_type\":\"upsert\",\"cells\":[\"service-id-4\",\"service-name-4\"]}}");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_api_db_schema() -> eyre::Result<()> {
        _ = tracing_subscriber::fmt::try_init();
        let (tripwire, _tripwire_worker, _tripwire_tx) = Tripwire::new_simple();

        let dir = tempfile::tempdir()?;

        let pool = SplitPool::create(dir.path().join("./test.sqlite"), tripwire.clone()).await?;

        {
            let mut conn = pool.write_priority().await?;
            migrate(&mut conn)?;
        }

        let (tx_bcast, _rx_bcast) = channel(1);
        let (tx_apply, _rx_apply) = channel(1);

        let agent = Agent::new(corro_types::agent::AgentConfig {
            actor_id: ActorId(Uuid::new_v4()),
            pool,
            config: ArcSwap::from_pointee(
                Config::builder()
                    .db_path(dir.path().join("corrosion.db").display().to_string())
                    .gossip_addr("127.0.0.1:1234".parse()?)
                    .api_addr("127.0.0.1:8080".parse()?)
                    .build()?,
            ),
            gossip_addr: "127.0.0.1:0".parse().unwrap(),
            api_addr: "127.0.0.1:0".parse().unwrap(),
            members: Default::default(),
            clock: Default::default(),
            bookie: Default::default(),
            subscribers: Default::default(),
            tx_bcast,
            tx_apply,
            schema: Default::default(),
            tripwire,
        });

        let (status_code, _body) = api_v1_db_schema(
            Extension(agent.clone()),
            axum::Json(vec![
                "CREATE TABLE tests (id BIGINT PRIMARY KEY, foo TEXT);".into(),
            ]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        // scope the schema reader in here
        {
            let schema = agent.schema().read();
            let tests = schema
                .tables
                .get("tests")
                .expect("no tests table in schema");

            let id_col = tests.columns.get("id").unwrap();
            assert_eq!(id_col.name, "id");
            assert_eq!(id_col.sql_type, SqliteType::Integer);
            assert_eq!(id_col.nullable, true);
            assert_eq!(id_col.primary_key, true);

            let foo_col = tests.columns.get("foo").unwrap();
            assert_eq!(foo_col.name, "foo");
            assert_eq!(foo_col.sql_type, SqliteType::Text);
            assert_eq!(foo_col.nullable, true);
            assert_eq!(foo_col.primary_key, false);
        }

        let (status_code, _body) = api_v1_db_schema(
            Extension(agent.clone()),
            axum::Json(vec![
                "CREATE TABLE tests2 (id BIGINT PRIMARY KEY, foo TEXT);".into(),
                "CREATE TABLE tests (id BIGINT PRIMARY KEY, foo TEXT);".into(),
            ]),
        )
        .await;

        assert_eq!(status_code, StatusCode::OK);

        let schema = agent.schema().read();
        let tests = schema
            .tables
            .get("tests")
            .expect("no tests table in schema");

        let id_col = tests.columns.get("id").unwrap();
        assert_eq!(id_col.name, "id");
        assert_eq!(id_col.sql_type, SqliteType::Integer);
        assert_eq!(id_col.nullable, true);
        assert_eq!(id_col.primary_key, true);

        let foo_col = tests.columns.get("foo").unwrap();
        assert_eq!(foo_col.name, "foo");
        assert_eq!(foo_col.sql_type, SqliteType::Text);
        assert_eq!(foo_col.nullable, true);
        assert_eq!(foo_col.primary_key, false);

        let tests = schema
            .tables
            .get("tests2")
            .expect("no tests2 table in schema");

        let id_col = tests.columns.get("id").unwrap();
        assert_eq!(id_col.name, "id");
        assert_eq!(id_col.sql_type, SqliteType::Integer);
        assert_eq!(id_col.nullable, true);
        assert_eq!(id_col.primary_key, true);

        let foo_col = tests.columns.get("foo").unwrap();
        assert_eq!(foo_col.name, "foo");
        assert_eq!(foo_col.sql_type, SqliteType::Text);
        assert_eq!(foo_col.nullable, true);
        assert_eq!(foo_col.primary_key, false);

        Ok(())
    }

    #[test]
    fn test_change_chunker() {
        // empty interator
        let mut chunker = ChunkedChanges::new(vec![].into_iter(), 0, 100, 50);

        assert_eq!(chunker.next(), Some(Ok((vec![], 0..=100))));
        assert_eq!(chunker.next(), None);

        let seq_0 = Change {
            seq: 0,
            ..Default::default()
        };
        let seq_1 = Change {
            seq: 1,
            ..Default::default()
        };
        let seq_2 = Change {
            seq: 2,
            ..Default::default()
        };

        // 2 iterations
        let mut chunker = ChunkedChanges::new(
            vec![Ok(seq_0.clone()), Ok(seq_1.clone()), Ok(seq_2.clone())].into_iter(),
            0,
            100,
            2,
        );

        assert_eq!(
            chunker.next(),
            Some(Ok((vec![seq_0.clone(), seq_1.clone()], 0..=1)))
        );
        assert_eq!(chunker.next(), Some(Ok((vec![seq_2.clone()], 2..=100))));
        assert_eq!(chunker.next(), None);

        let mut chunker = ChunkedChanges::new(
            vec![Ok(seq_0.clone()), Ok(seq_1.clone())].into_iter(),
            0,
            0,
            1,
        );

        assert_eq!(chunker.next(), Some(Ok((vec![seq_0.clone()], 0..=0))));
        assert_eq!(chunker.next(), None);
    }
}
