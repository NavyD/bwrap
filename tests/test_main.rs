use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use assert_cmd::{Command, assert::OutputAssertExt};
use bwrap::bwserve_api::{
    BWServeGetRespData, BWServeResp, BWServeStatusRespData,
};
use httpmock::Method::POST;
use httpmock::{Method::GET, MockServer};
use jsonpath_rust::JsonPath;
use predicates::prelude::*;
use rstest::{fixture, rstest};
use serde_json::{
    Value, from_str as json_dec, from_value as json_dec_value, json,
    to_string as json_enc,
};
use similar_asserts::assert_eq;

async fn bwcmd<I, S, B>(args: I, body: B) -> Command
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

    let mut cmd = Command::cargo_bin("bw").expect("not found cargo bin");
    cmd.args(["--api-url", &server.url("/")])
        .args(args.iter().map(AsRef::as_ref));
    cmd
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
    let out = bwcmd(["get", "item", "github.com"], body).await.unwrap();
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
    let data = BWServeResp {
        success: false,
        message: Some("Not found.".to_string()),
        data: None::<String>,
    };
    bwcmd(["get", "item", "xxx"], json_enc(&data).unwrap())
        .await
        .env("RUST_LOG", "off")
        .output()
        .unwrap()
        .assert()
        .code(predicate::eq(1))
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::diff(data.message.to_owned().unwrap()).trim());
}

#[tokio::test]
async fn bw_unlock_when_bw_error_test() {
    let exitcode = 111;
    let bw_path = gen_mock_bw().exitcode(exitcode).call().unwrap();
    let args = vec!["unlock", "--bw-path", bw_path.to_str().unwrap()];
    let mut cmd = Command::cargo_bin("bw").expect("not found cargo bin");
    cmd.args(args)
        .output()
        .unwrap()
        .assert()
        .code(predicate::eq(exitcode as i32));
}

static TMPDIR: LazyLock<tempfile::TempDir> =
    LazyLock::new(|| tempfile::Builder::new().prefix("bw-").tempdir().unwrap());

#[derive(Clone, bon::Builder)]
#[builder(on(String, into))]
struct MockBW {
    stdout: Option<String>,
    stderr: Option<String>,
    #[builder(default = 0)]
    exitcode: u8,
}
fn mock_bw_path(bw: MockBW) -> Result<PathBuf> {
    gen_mock_bw()
        .maybe_stdout(bw.stdout)
        .maybe_stderr(bw.stderr)
        .exitcode(bw.exitcode)
        .call()
}

#[bon::builder]
#[builder(on(String, into))]
#[cfg(unix)]
fn gen_mock_bw(
    stdout: Option<String>,
    stderr: Option<String>,
    exitcode: Option<u8>,
) -> Result<PathBuf> {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string();
    let bw_path = TMPDIR.path().join(format!("bw-{}.sh", id));
    let mut bw_sh_file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .mode(0o755)
        .open(&bw_path)?;
    let stdout_path = bw_path.with_added_extension("stdout");
    let stderr_path = bw_path.with_added_extension("stderr");
    let mut args = vec![];
    if let Some(s) = stdout {
        fs::write(&stdout_path, &s)?;
        args.extend_from_slice(&[
            "--stdout-file",
            stdout_path.to_str().with_context(|| s)?,
        ]);
    }
    if let Some(s) = stderr {
        fs::write(&stderr_path, &s)?;
        args.extend_from_slice(&[
            "--stderr-file",
            stderr_path.to_str().with_context(|| s)?,
        ]);
    }
    let exitcode = exitcode.unwrap_or(0).to_string();
    args.extend_from_slice(&["--exitcode", &exitcode]);

    writeln!(
        bw_sh_file,
        r#"#!/usr/bin/sh
exec "{}" {} -- "$@"
"#,
        env!("CARGO_BIN_EXE_mockbw"),
        args.join(" ")
    )?;

    // 避免出现 zsh: text file busy: /tmp/bw-j16A3i.mock.sh/bw.sh
    drop(bw_sh_file);
    Ok(bw_path)
}

#[tokio::test]
#[cfg(unix)]
async fn bw_unlock_stop_daemon_test() {
    let stdout = "bw-session-xx";
    let bw_path = gen_mock_bw().stdout(stdout).call().unwrap();
    // println!("bw_path={:?}", bw_path);
    // sleep(Duration::from_secs(30)).await;

    let args = vec!["unlock", "--bw-path", bw_path.to_str().unwrap()];
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/__bwrap/shutdown");
            then.status(200);
        })
        .await;

    let mut cmd = Command::cargo_bin("bw").expect("not found cargo bin");
    cmd.args(args)
        .output()
        .unwrap()
        .assert()
        .success()
        .stdout(predicate::str::diff(stdout));
}

const ENV_RUST_LOG: &str = "error,bwrap=trace,bw=trace";

#[test]
#[cfg(unix)]
fn bw_unlock_restart_test() {
    let stdout = "bw-session-xx";
    let bw_path = gen_mock_bw().stdout(stdout).call().unwrap();

    let args = vec![
        "unlock",
        "--raw",
        "--restart",
        "--bw-path",
        bw_path.to_str().unwrap(),
    ];
    let res = std::panic::catch_unwind(|| {
        Command::cargo_bin("bw")
            .expect("not found cargo bin")
            .args(args)
            .env("RUST_LOG", ENV_RUST_LOG)
            .output()
            .unwrap()
            .assert()
            .success()
            .stdout(predicate::str::diff(stdout).trim());
    });
    Command::cargo_bin("bw")
        .expect("not found cargo bin")
        .args(["serve", "--stop"])
        .output()
        .unwrap()
        .assert()
        .success();
    assert!(res.is_ok(), "result is error: {:?}", res);
}

const BIN_NAME: &str = "bw";

#[rstest]
#[case(MockBW::builder().build(), &["sync"])]
#[case(MockBW::builder().stdout("Your vault is locked.").build(), &["lock"])]
#[case(
    MockBW::builder().exitcode(121).stderr("unknown-subcmd error").build(),
    &["unknown-subcmd"]
)]
fn bw_external_test(#[case] bw: MockBW, #[case] args: &[&str]) {
    let bw_path = mock_bw_path(bw.clone()).unwrap();
    Command::cargo_bin(BIN_NAME)
        .unwrap()
        .env("RUST_LOG", "off")
        .args(["--bw-path", bw_path.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
        .assert()
        .code(predicate::eq(bw.exitcode as i32))
        .stderr(predicate::str::diff(bw.stderr.unwrap_or("".to_string())))
        .stdout(predicate::str::diff(bw.stdout.unwrap_or("".to_string())));
}
