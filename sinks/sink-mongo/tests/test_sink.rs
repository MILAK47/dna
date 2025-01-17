use apibara_core::node::v1alpha2::{Cursor, DataFinality};
use apibara_sink_common::{Context, CursorAction, Sink};
use apibara_sink_mongo::{MongoSink, SinkMongoError, SinkMongoOptions};
use error_stack::{Result, ResultExt};
use futures_util::TryStreamExt;
use mongodb::{
    bson::{doc, to_document, Bson, Document},
    options::FindOptions,
    Collection,
};
use serde_json::{json, Value};
use testcontainers::{clients, core::WaitFor, GenericImage};

fn new_mongo_image() -> GenericImage {
    GenericImage::new("mongo", "7.0.1")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
}

fn new_cursor(order_key: u64) -> Cursor {
    Cursor {
        order_key,
        unique_key: order_key.to_be_bytes().to_vec(),
    }
}

fn new_batch(start_cursor: &Option<Cursor>, end_cursor: &Cursor) -> Value {
    new_batch_with_extra(start_cursor, end_cursor, json!({}))
}

fn new_batch_with_extra(start_cursor: &Option<Cursor>, end_cursor: &Cursor, extra: Value) -> Value {
    let mut batch = Vec::new();

    let start_block_num = match start_cursor {
        Some(cursor) => cursor.order_key,
        None => 0,
    };

    let end_block_num = end_cursor.order_key;

    for i in start_block_num..end_block_num {
        let mut doc = json!({
            "block_num": i,
            "block_str": format!("block_{}", i),
        });

        doc.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone().into_iter());

        batch.push(doc);
    }

    json!(batch)
}

fn new_not_array_of_objects() -> Value {
    json!([0, { "key": "value" }, 1])
}

fn new_docs(start_cursor: &Option<Cursor>, end_cursor: &Cursor) -> Vec<Document> {
    let mut batch = Vec::new();

    let start_block_num = match start_cursor {
        Some(cursor) => cursor.order_key,
        None => 0,
    };

    let end_block_num = end_cursor.order_key;

    for i in start_block_num..end_block_num {
        batch.push(
            // we have to convert first to a json then to a mongo document for
            // the numbers to be handled as u64, doc! macro don't handle u64
            // for some reason
            to_document(&json!({
                "block_num": i,
                "block_str": format!("block_{}", i),
                "_cursor": json!({"from": end_block_num}),
            }))
            .unwrap(),
        );
    }
    batch
}

async fn get_all_docs(collection: &Collection<Document>) -> Vec<Document> {
    let find_options = Some(
        FindOptions::builder()
            .projection(Some(doc! {"_id": 0}))
            .build(),
    );

    collection
        .find(None, find_options)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn test_handle_data() -> Result<(), SinkMongoError> {
    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        ..SinkMongoOptions::default()
    };

    let mut sink = MongoSink::from_options(options).await?;

    let batch_size = 2;
    let num_batches = 5;

    let mut all_docs = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);
        let ctx = Context {
            cursor: cursor.clone(),
            end_cursor: end_cursor.clone(),
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);

        all_docs.extend(new_docs(&cursor, &end_cursor));

        let action = sink.handle_data(&ctx, &new_not_array_of_objects()).await?;

        assert_eq!(action, CursorAction::Persist);

        let action = sink.handle_data(&ctx, &json!([])).await?;

        assert_eq!(action, CursorAction::Persist);
    }

    assert_eq!(all_docs, get_all_docs(&sink.collection).await);

    Ok(())
}

async fn test_handle_invalidate_all(
    invalidate_from: &Option<Cursor>,
) -> Result<(), SinkMongoError> {
    assert!(invalidate_from.is_none() || invalidate_from.clone().unwrap().order_key == 0);

    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        ..SinkMongoOptions::default()
    };

    let mut sink = MongoSink::from_options(options).await?;

    let batch_size = 2;
    let num_batches = 5;

    let mut all_docs: Vec<Document> = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);
        let ctx = Context {
            cursor: cursor.clone(),
            end_cursor: end_cursor.clone(),
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        assert_eq!(action, CursorAction::Persist);

        all_docs.extend(new_docs(&cursor, &end_cursor));
    }

    assert_eq!(all_docs, get_all_docs(&sink.collection).await);

    sink.handle_invalidate(invalidate_from).await?;
    assert_eq!(Vec::<Document>::new(), get_all_docs(&sink.collection).await);

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_genesis() -> Result<(), SinkMongoError> {
    test_handle_invalidate_all(&None).await
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_block_zero() -> Result<(), SinkMongoError> {
    test_handle_invalidate_all(&Some(new_cursor(0))).await
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate() -> Result<(), SinkMongoError> {
    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        ..SinkMongoOptions::default()
    };

    let mut sink = MongoSink::from_options(options).await?;

    let batch_size = 2;
    let num_batches = 5;

    let mut all_docs: Vec<Document> = vec![];

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let batch = new_batch(&cursor, &end_cursor);
        let ctx = Context {
            cursor: cursor.clone(),
            end_cursor: end_cursor.clone(),
            finality,
        };

        let action = sink.handle_data(&ctx, &batch).await?;

        all_docs.extend(new_docs(&cursor, &end_cursor));

        assert_eq!(action, CursorAction::Persist);
    }

    assert_eq!(all_docs, get_all_docs(&sink.collection).await);

    let invalidate_from = 2;

    sink.handle_invalidate(&Some(new_cursor(invalidate_from)))
        .await?;

    let expected_docs: Vec<Document> = all_docs
        .into_iter()
        .filter(|doc| {
            doc.get_document("_cursor")
                .unwrap()
                .get_i64("from")
                .unwrap() as u64
                <= invalidate_from
        })
        .collect();

    assert_eq!(expected_docs, get_all_docs(&sink.collection).await);

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_with_extra_condition() -> Result<(), SinkMongoError> {
    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        invalidate: Some(doc! { "col1": "a", "col2": "a" }),
        ..SinkMongoOptions::default()
    };

    let mut sink = MongoSink::from_options(options).await?;

    let batch_size = 2;
    let num_batches = 5;

    let mut docs_count = 0;

    for order_key in 0..num_batches {
        let cursor = Some(new_cursor(order_key * batch_size));
        let end_cursor = new_cursor((order_key + 1) * batch_size);
        let finality = DataFinality::DataStatusFinalized;
        let ctx = Context {
            cursor: cursor.clone(),
            end_cursor: end_cursor.clone(),
            finality,
        };

        {
            let batch =
                new_batch_with_extra(&cursor, &end_cursor, json!({ "col1": "a", "col2": "a" }));
            docs_count += batch_size;
            let action = sink.handle_data(&ctx, &batch).await?;
            assert_eq!(action, CursorAction::Persist);
        }

        {
            let batch =
                new_batch_with_extra(&cursor, &end_cursor, json!({ "col1": "a", "col2": "b" }));
            docs_count += batch_size;
            let action = sink.handle_data(&ctx, &batch).await?;
            assert_eq!(action, CursorAction::Persist);
        }
    }

    assert_eq!(docs_count, batch_size * num_batches * 2);

    let invalidate_from = 2;

    sink.handle_invalidate(&Some(new_cursor(invalidate_from)))
        .await?;

    // We expect all documents for `{col1: a, col2: b}`, but only
    // one batch worth of data for `{col1: a, col2: a}`.
    let expected_docs_count = (batch_size * num_batches) + (batch_size * (invalidate_from - 1));
    assert_eq!(
        expected_docs_count,
        get_all_docs(&sink.collection).await.len() as u64
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_data_in_entity_mode() -> Result<(), SinkMongoError> {
    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        entity_mode: Some(true),
        invalidate: None,
    };

    let mut sink = MongoSink::from_options(options).await?;
    let finality = DataFinality::DataStatusFinalized;

    {
        // Insert the first batch.
        // Note that (0x1, 2) has duplicate items. Technically this is allowed but values will be
        // squashed.
        let cursor = Some(new_cursor(0));
        let end_cursor = new_cursor(1);
        let batch = json!([
            json!({"entity": { "address": "0x1", "token_id": "1", }, "update": { "$set": { "v0": "a", "v1": "a" } } }),
            json!({"entity": { "address": "0x1", "token_id": "2", }, "update": { "$set": { "v0": "b", "v1": "b" } } }),
            json!({"entity": { "address": "0x1", "token_id": "2", }, "update": [{ "$set": { "v0": "a", "v1": "a" } }] }),
            json!({"entity": { "address": "0x1", "token_id": "3", }, "update": [{ "$set": { "v0": "a", "v1": "a" } }] }),
        ]);

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        sink.handle_data(&ctx, &batch).await?;
    }

    {
        // Update some values for some entities.
        let cursor = Some(new_cursor(1));
        let end_cursor = new_cursor(2);
        let batch = json!([
            json!({"entity": { "address": "0x1", "token_id": "1", }, "update": { "$set": { "v1": "b"}, "$inc": { "v2": 7 } } }),
            json!({"entity": { "address": "0x1", "token_id": "2", }, "update": [{ "$set": { "v0": "b"} }] }),
        ]);

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        sink.handle_data(&ctx, &batch).await?;

        // Check that the values were updated correctly.
        // For example, we check that key v0 is still present.

        let new_docs = sink
            .collection
            .find(
                Some(doc! {"_cursor.to": Bson::Null, "address": "0x1", "token_id": "1" }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
        let new_doc = &new_docs[0];
        assert_eq!(new_doc.get_str("v0").unwrap(), "a");
        assert_eq!(new_doc.get_str("v1").unwrap(), "b");
        assert_eq!(new_doc.get_i64("v2").unwrap(), 7);

        let new_docs = sink
            .collection
            .find(
                Some(doc! {"_cursor.to": Bson::Null, "address": "0x1", "token_id": "2" }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
    }

    {
        // Update a single entity and insert a new one.
        let cursor = Some(new_cursor(2));
        let end_cursor = new_cursor(3);
        let batch = json!([
            json!({ "entity": { "address": "0x1", "token_id": "1" }, "update": { "$set": { "v1": "c" } } }),
            json!({ "entity": { "address": "0x1", "token_id": "4" }, "update": { "$set": { "v0": "a", "v1": "a" } } }),
        ]);

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        sink.handle_data(&ctx, &batch).await?;

        let updated_docs = sink
            .collection
            .find(
                Some(doc! {"_cursor.to": Bson::Null, "address": "0x1", "token_id": "1" }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(updated_docs.len(), 1);
        let updated_doc = &updated_docs[0];
        assert_eq!(updated_doc.get_str("v0").unwrap(), "a");
        assert_eq!(updated_doc.get_str("v1").unwrap(), "c");

        let new_docs = sink
            .collection
            .find(
                Some(doc! {"_cursor.to": Bson::Null, "address": "0x1", "token_id": "4" }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
        let new_doc = &new_docs[0];
        assert_eq!(new_doc.get_str("v0").unwrap(), "a");
        assert_eq!(new_doc.get_str("v1").unwrap(), "a");
    }

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_handle_invalidate_in_entity_mode() -> Result<(), SinkMongoError> {
    let docker = clients::Cli::default();
    let mongo = docker.run(new_mongo_image());
    let port = mongo.get_host_port_ipv4(27017);

    let options = SinkMongoOptions {
        connection_string: Some(format!("mongodb://localhost:{}", port)),
        database: Some("test".into()),
        collection_name: Some("test".into()),
        entity_mode: Some(true),
        invalidate: None,
    };

    let mut sink = MongoSink::from_options(options).await?;
    let finality = DataFinality::DataStatusFinalized;

    {
        let cursor = Some(new_cursor(0));
        let end_cursor = new_cursor(1);
        let batch = json!([
            json!({ "entity": { "address": "0x1", "token_id": "1" }, "update": { "$set": { "v0": "a", "v1": "a" } } }),
            json!({ "entity": { "address": "0x1", "token_id": "2" }, "update": { "$set": { "v0": "a", "v1": "a"} } }),
        ]);

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        sink.handle_data(&ctx, &batch).await?;
    }

    {
        let cursor = Some(new_cursor(1));
        let end_cursor = new_cursor(2);
        let batch = json!([
            json!({ "entity": { "address": "0x1", "token_id": "2" }, "update": { "$set": { "v1": "b" } } }),
        ]);

        let ctx = Context {
            cursor,
            end_cursor,
            finality,
        };

        sink.handle_data(&ctx, &batch).await?;

        let new_docs = sink
            .collection
            .find(
                Some(doc! { "token_id": "2", "_cursor.to": Bson::Null }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
        let new_doc = &new_docs[0];
        assert_eq!(new_doc.get_str("v0").unwrap(), "a");
        assert_eq!(new_doc.get_str("v1").unwrap(), "b");
    }

    {
        // This actually shouldn't invalidate any data since the new heade is the same as before
        // (2), but it catches off by one errors in the invalidation logic.
        let new_head = Some(new_cursor(2));
        sink.handle_invalidate(&new_head).await?;

        let new_docs = sink
            .collection
            .find(
                Some(doc! { "token_id": "2", "_cursor.to": Bson::Null }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
        let new_doc = &new_docs[0];
        assert_eq!(new_doc.get_str("v0").unwrap(), "a");
        assert_eq!(new_doc.get_str("v1").unwrap(), "b");
    }

    {
        // Now actually invalidate data.
        let new_head = Some(new_cursor(1));
        sink.handle_invalidate(&new_head).await?;

        let new_docs = sink
            .collection
            .find(
                Some(doc! { "token_id": "2", "_cursor.to": Bson::Null }),
                None,
            )
            .await
            .change_context(SinkMongoError)?
            .try_collect::<Vec<_>>()
            .await
            .change_context(SinkMongoError)?;

        assert_eq!(new_docs.len(), 1);
        let new_doc = &new_docs[0];
        assert_eq!(new_doc.get_str("v0").unwrap(), "a");
        assert_eq!(new_doc.get_str("v1").unwrap(), "a");
    }

    Ok(())
}
