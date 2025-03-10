// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use api::v1::greptime_request::Request;
use api::v1::InsertRequest;
use async_trait::async_trait;
use axum::{http, Router};
use axum_test_helper::TestClient;
use common_query::Output;
use datatypes::schema::Schema;
use query::parser::PromQuery;
use servers::error::{Error, Result};
use servers::http::{HttpOptions, HttpServerBuilder};
use servers::influxdb::InfluxdbRequest;
use servers::query_handler::grpc::GrpcQueryHandler;
use servers::query_handler::sql::SqlQueryHandler;
use servers::query_handler::InfluxdbLineProtocolHandler;
use session::context::QueryContextRef;
use tokio::sync::mpsc;

use crate::auth::{DatabaseAuthInfo, MockUserProvider};

struct DummyInstance {
    tx: Arc<mpsc::Sender<(String, String)>>,
}

#[async_trait]
impl GrpcQueryHandler for DummyInstance {
    type Error = Error;

    async fn do_query(
        &self,
        _query: Request,
        _ctx: QueryContextRef,
    ) -> std::result::Result<Output, Self::Error> {
        unimplemented!()
    }
}

#[async_trait]
impl InfluxdbLineProtocolHandler for DummyInstance {
    async fn exec(&self, request: &InfluxdbRequest, ctx: QueryContextRef) -> Result<()> {
        let requests: Vec<InsertRequest> = request.try_into()?;

        for expr in requests {
            let _ = self.tx.send((ctx.current_schema(), expr.table_name)).await;
        }

        Ok(())
    }
}

#[async_trait]
impl SqlQueryHandler for DummyInstance {
    type Error = Error;

    async fn do_query(&self, _: &str, _: QueryContextRef) -> Vec<Result<Output>> {
        unimplemented!()
    }

    async fn do_promql_query(
        &self,
        _: &PromQuery,
        _: QueryContextRef,
    ) -> Vec<std::result::Result<Output, Self::Error>> {
        unimplemented!()
    }

    async fn do_describe(
        &self,
        _stmt: sql::statements::statement::Statement,
        _query_ctx: QueryContextRef,
    ) -> Result<Option<Schema>> {
        unimplemented!()
    }

    async fn is_valid_schema(&self, _catalog: &str, _schema: &str) -> Result<bool> {
        Ok(true)
    }
}

fn make_test_app(tx: Arc<mpsc::Sender<(String, String)>>, db_name: Option<&str>) -> Router {
    let instance = Arc::new(DummyInstance { tx });
    let mut server_builder = HttpServerBuilder::new(HttpOptions::default());
    server_builder.with_sql_handler(instance.clone());
    server_builder.with_grpc_handler(instance.clone());
    let mut user_provider = MockUserProvider::default();
    if let Some(name) = db_name {
        user_provider.set_authorization_info(DatabaseAuthInfo {
            catalog: "greptime",
            schema: name,
            username: "greptime",
        })
    }
    server_builder.with_user_provider(Arc::new(user_provider));

    server_builder.with_influxdb_handler(instance);
    let server = server_builder.build();
    server.make_app()
}

#[tokio::test]
async fn test_influxdb_write() {
    let (tx, mut rx) = mpsc::channel(100);
    let tx = Arc::new(tx);

    let app = make_test_app(tx.clone(), None);
    let client = TestClient::new(app);

    let result = client.get("/v1/influxdb/health").send().await;
    assert_eq!(result.status(), 200);

    let result = client.get("/v1/influxdb/ping").send().await;
    assert_eq!(result.status(), 204);

    // right request
    let result = client
        .post("/v1/influxdb/write?db=public")
        .body("monitor,host=host1 cpu=1.2 1664370459457010101")
        .header(http::header::AUTHORIZATION, "token greptime:greptime")
        .send()
        .await;
    assert_eq!(result.status(), 204);
    assert!(result.text().await.is_empty());

    // right request using v1 auth
    let result = client
        .post("/v1/influxdb/write?db=public&p=greptime&u=greptime")
        .body("monitor,host=host1 cpu=1.2 1664370459457010101")
        .send()
        .await;
    assert_eq!(result.status(), 204);
    assert!(result.text().await.is_empty());

    // wrong pwd
    let result = client
        .post("/v1/influxdb/write?db=public")
        .body("monitor,host=host1 cpu=1.2 1664370459457010101")
        .header(http::header::AUTHORIZATION, "token greptime:wrongpwd")
        .send()
        .await;
    assert_eq!(result.status(), 401);

    // no auth
    let result = client
        .post("/v1/influxdb/write?db=public")
        .body("monitor,host=host1 cpu=1.2 1664370459457010101")
        .send()
        .await;
    assert_eq!(result.status(), 401);

    // make new app for db=influxdb
    let app = make_test_app(tx, Some("influxdb"));
    let client = TestClient::new(app);

    // right request
    let result = client
        .post("/v1/influxdb/write?db=influxdb")
        .body("monitor,host=host1 cpu=1.2 1664370459457010101")
        .header(http::header::AUTHORIZATION, "token greptime:greptime")
        .send()
        .await;
    assert_eq!(result.status(), 204);
    assert!(result.text().await.is_empty());

    // bad request
    let result = client
        .post("/v1/influxdb/write?db=influxdb")
        .body("monitor,   host=host1 cpu=1.2 1664370459457010101")
        .header(http::header::AUTHORIZATION, "token greptime:greptime")
        .send()
        .await;
    assert_eq!(result.status(), 400);
    assert!(!result.text().await.is_empty());

    let mut metrics = vec![];
    while let Ok(s) = rx.try_recv() {
        metrics.push(s);
    }
    assert_eq!(
        metrics,
        vec![
            ("public".to_string(), "monitor".to_string()),
            ("public".to_string(), "monitor".to_string()),
            ("influxdb".to_string(), "monitor".to_string())
        ]
    );
}
