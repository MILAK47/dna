use apibara_core::node::v1alpha2::{Cursor, DataFinality};
use apibara_sink_common::{Context, CursorAction, Sink};
use apibara_sink_postgres::{
    InvalidateColumn, PostgresSink, SinkPostgresError, SinkPostgresOptions,
};
use error_stack::Result;
use serde_json::{json, Value};
use testcontainers::{clients, core::WaitFor, GenericImage};
use tokio_postgres::{Client, NoTls};

fn new_postgres_image() -> GenericImage {
    GenericImage::new("postgres", "15-alpine")
        .with_exposed_port(5432)
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
}

fn new_cursor(order_key: u64) -> Cursor {
    Cursor {
        order_key,
        unique_key: order_key.to_be_bytes().to_vec(),
    }
}

fn new_batch(start_cursor: &Option<Cursor>, end_cursor: &Cursor) -> Value {
    new_batch_with_additional_columns(start_cursor, end_cursor, None, None)
}

fn new_batch_with_additional_columns(
    start_cursor: &Option<Cursor>,
    end_cursor: &Cursor,
    col1: Option<String>,
    col2: Option<String>,
) -> Value {
    let mut batch = Vec::new();

    let start_block_num = match start_cursor {
        Some(cursor) => cursor.order_key,
        None => 0,
    };

    let end_block_num = end_cursor.order_key;

    for i in start_block_num..end_block_num {
        batch.push(json!({
            "block_num": i,
            "block_str": format!("block_{}", i),
            "col1": col1,
            "col2": col2,
        }));
    }
    json!(batch)
}

fn new_not_array_of_objects() -> Value {
    json!([0, { "key": "value" }, 1])
}

#[derive(Debug, PartialEq)]
struct TestRow {
    pub cursor: i64,
    pub block_num: i32,
    pub block_str: String,
}

fn new_rows(start_cursor: &Option<Cursor>, end_cursor: &Cursor) -> Vec<TestRow> {
    let start_block_num = match start_cursor {
        Some(cursor) => cursor.order_key as i64,
        None => 0,
    };

    let end_block_num = end_cursor.order_key as i64;

    (start_block_num..end_block_num)
        .map(|i| TestRow {
            block_num: i as i32,
            cursor: end_block_num,
            block_str: format!("block_{}", i),
        })
        .collect()
}

async fn get_all_rows(client: &Client) -> Vec<TestRow> {
    let rows = client.query("SELECT * From test", &[]).await.unwrap();

    rows.into_iter()
        .map(|row| {
            let cursor: i64 = row.get("_cursor");
            let block_num: i32 = row.get("block_num");
            let block_str: String = row.get("block_str");

            TestRow {
                cursor,
                block_num,
                block_str,
            }
        })
        .collect()
}

async fn get_num_rows(client: &Client) -> i64 {
    let rows = client
        .query("SELECT count(*) From test", &[])
        .await
        .unwrap();
    rows[0].get(0)
}

async fn create_test_table(port: u16) {
    let create_table_query =
        "CREATE TABLE test(block_num int, block_str varchar(10), col1 text, col2 text, _cursor bigint);";

    let connection_string = format!("postgresql://postgres@localhost:{}", port);
    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .unwrap();

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    client.query(create_table_query, &[]).await.unwrap();
}

async fn new_sink(port: u16) -> PostgresSink {
    new_sink_with_invalidate(port, None).await
}

async fn new_sink_with_invalidate(
    port: u16,
    invalidate: Option<Vec<InvalidateColumn>>,
) -> PostgresSink {
    let options = SinkPostgresOptions {
        connection_string: Some(format!("postgresql://postgres@localhost:{}", port)),
        table_name: Some("test".into()),
        no_tls: Some(true),
        invalidate,
        ..Default::default()
    };
    PostgresSink::from_options(options).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn test_handle_data() -> Result<(), SinkPostgresError> {
    let docker = clients::Cli::default();
    let postgres = docker.run(new_postgres_image());
    let port = postgres.get_host_port_ipv4(5432);

    create_test_table(port).await;

    let mut sink = new_sink(port).await;

    let batch_size = 2;
    let num_batches = 5;

    let mut all_rows = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);

        all_rows.extend(new_rows(&cursor, &end_cursor));

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);

        let action = sink.handle_data(&ctx, &new_not_array_of_objects()).await?;

        assert_eq!(action, CursorAction::Persist);

        let action = sink.handle_data(&ctx, &json!([])).await?;

        assert_eq!(action, CursorAction::Persist);
    }

    assert_eq!(all_rows, get_all_rows(&sink.client).await);

    Ok(())
}

async fn test_handle_invalidate_all(
    invalidate_from: &Option<Cursor>,
) -> Result<(), SinkPostgresError> {
    assert!(invalidate_from.is_none() || invalidate_from.clone().unwrap().order_key == 0);

    let docker = clients::Cli::default();
    let postgres = docker.run(new_postgres_image());
    let port = postgres.get_host_port_ipv4(5432);

    create_test_table(port).await;

    let mut sink = new_sink(port).await;

    let batch_size = 2;
    let num_batches = 5;

    let mut expected_rows = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);

        expected_rows.extend(new_rows(&cursor, &end_cursor));

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);
    }

    assert_eq!(expected_rows, get_all_rows(&sink.client).await);

    let num_rows = get_num_rows(&sink.client).await as u64;
    assert_eq!(num_rows, batch_size * num_batches);

    sink.handle_invalidate(invalidate_from).await?;

    assert_eq!(get_num_rows(&sink.client).await, 0);

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_genesis() -> Result<(), SinkPostgresError> {
    test_handle_invalidate_all(&None).await
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_block_zero() -> Result<(), SinkPostgresError> {
    test_handle_invalidate_all(&Some(new_cursor(0))).await
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate() -> Result<(), SinkPostgresError> {
    let docker = clients::Cli::default();
    let postgres = docker.run(new_postgres_image());
    let port = postgres.get_host_port_ipv4(5432);

    create_test_table(port).await;

    let mut sink = new_sink(port).await;

    let batch_size = 2;
    let num_batches = 5;

    let mut all_rows = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);

        all_rows.extend(new_rows(&cursor, &end_cursor));

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);
    }

    assert_eq!(all_rows, get_all_rows(&sink.client).await);

    let invalidate_from = 2;

    sink.handle_invalidate(&Some(new_cursor(invalidate_from as u64)))
        .await?;

    let expected_rows: Vec<TestRow> = all_rows
        .into_iter()
        .filter(|row| row.cursor <= invalidate_from)
        .collect();

    assert_eq!(expected_rows, get_all_rows(&sink.client).await);

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_with_additional_condition() -> Result<(), SinkPostgresError> {
    let docker = clients::Cli::default();
    let postgres = docker.run(new_postgres_image());
    let port = postgres.get_host_port_ipv4(5432);

    create_test_table(port).await;

    let invalidate = vec![
        InvalidateColumn {
            column: "col1".into(),
            value: "a".into(),
        },
        InvalidateColumn {
            column: "col2".into(),
            value: "a".into(),
        },
    ];
    let mut sink = new_sink_with_invalidate(port, Some(invalidate)).await;

    let batch_size = 2;
    let num_batches = 5;

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;

        let batch = new_batch_with_additional_columns(
            &cursor,
            &end_cursor,
            Some("a".into()),
            Some("b".into()),
        );

        let ctx = Context {
            cursor: cursor.clone(),
            end_cursor: end_cursor.clone(),
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);

        let batch = new_batch_with_additional_columns(
            &cursor,
            &end_cursor,
            Some("a".into()),
            Some("a".into()),
        );

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);
    }

    sink.handle_invalidate(&Some(new_cursor(2))).await?;

    let rows = get_all_rows(&sink.client).await;
    // 10 rows with col1 = "a" and col2 = "b"
    // 2 rows with col1 = "a" and col2 = "a"
    assert_eq!(rows.len(), 12);

    Ok(())
}
