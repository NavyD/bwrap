use std::{fmt::Debug, process::Output};

use assert_cmd::assert::OutputAssertExt;
use bwrap::bwserve_api::{
    BWServeGetRespData, BWServeResp, BWServeStatusRespData,
};
use httpmock::{Method::GET, MockServer};
use jsonpath_rust::JsonPath;
use predicates::prelude::*;
use rstest::{fixture, rstest};
use serde_json::{
    Value, from_str as json_dec, from_value as json_dec_value, json,
    to_string as json_enc,
};
use similar_asserts::assert_eq;

async fn bwcmd<I, S, B>(args: I, body: B) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<str> + Debug,
    B: AsRef<[u8]>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            let url_path = match args[0].as_ref() {
                "get" => std::iter::once("/object")
                    .chain(args[1..].iter().map(AsRef::as_ref))
                    .collect::<Vec<_>>()
                    .join("/"),
                "status" => "/status".to_string(),
                _ => panic!("unsupported args={:?}", args),
            };
            when.method(GET).path(url_path);
            then.status(200).body(body);
        })
        .await;

    assert_cmd::Command::cargo_bin("bw")
        .expect("not found cargo bin")
        .args(["--api-url", &server.url("/")])
        .args(args.iter().map(AsRef::as_ref))
        .output()
        .unwrap()
}

#[fixture]
fn item_gh() -> Value {
    json!({
        "type": 1,
        "name": "github.com",
        "favorite": false,
        "reprompt": 0,
        "id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
        "collectionIds": [],
        "object": "item",
        "folderId": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
        "fields": [
        {
            "type": 0,
            "name": "PAT_GITEA",
            "value": "xxx_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        },
        {
            "type": 0,
            "name": "PAT_RSSHUB",
            "value": "xxx_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        },
        ],
        "login": {
            "uris": [{ "uri": "https://github.com" }],
            "fido2Credentials": [],
            "username": "xxxxxxxx@xxxxx.xxx",
            "password": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "totp": "xxxxxxxxxxxxxxxx",
            "passwordRevisionDate": null
        },
        "passwordHistory": [],
        "creationDate": "2023-11-09T17:31:37.377Z",
        "revisionDate": "2026-05-02T03:11:42.698Z",
        "attachments": []
    })
}

#[rstest]
#[tokio::test]
async fn bw_status_stdout_test(
    #[values("locked", "unlocked", "unauthenticated")] status: &str,
) {
    let data = json!({
        "serverUrl": "https://vault.example.com",
        "lastSync": "2026-08-04T02:07:01.434Z",
        "userEmail": "user@example.com",
        "userId": "45e630c6-ba45-4f7a-93a4-935376a025d8",
        "status": status,
    });
    let body = json_enc(&BWServeResp {
        success: true,
        data: Some(BWServeStatusRespData {
            object: "template".to_string(),
            template: json_dec_value(data.clone()).unwrap(),
        }),
        message: None,
    })
    .unwrap();
    bwcmd(["status"], body).await.assert().success().stdout(
        predicate::str::is_empty()
            .trim()
            .not()
            .and(predicate::function(|s| {
                json_dec::<Value>(s).unwrap() == data
            })),
    );
}

#[rstest]
#[tokio::test]
async fn bw_get_item_stdout_test(item_gh: Value) {
    let body = json_enc(&BWServeResp {
        success: true,
        message: None,
        data: Some(BWServeGetRespData::Item(Box::new(
            json_dec_value(item_gh.clone()).unwrap(),
        ))),
    })
    .unwrap();
    let out = bwcmd(["get", "item", "github.com"], body).await;
    let stdout_str = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&out.stderr).to_string();
    out.assert()
        .success()
        .stdout(predicate::str::is_empty().trim().not());
    let out_json_val: Value = json_dec(&stdout_str).expect(&stderr_str);
    let jsonpaths = [
        "$['name','id','object','folderId','fields']",
        "$.login['username', 'password', 'totp']",
    ];
    for jp in jsonpaths {
        assert_eq!(out_json_val.query(jp), item_gh.query(jp));
    }
}

#[rstest]
#[tokio::test]
async fn bw_get_item_error_test() {
    let body = json_enc(&BWServeResp {
        success: false,
        message: Some("Not found.".to_string()),
        data: None::<String>,
    })
    .unwrap();
    bwcmd(["get", "item", "xxx"], body).await.assert().failure();
}
